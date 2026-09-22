//! Fail-closed verification of signed PM UM history against separately authenticated
//! order snapshots. This is evidence preparation, never an OMS mutation or live
//! trading authorization. A caller MUST establish the original history anchor
//! independently and MUST persist fills and the cursor atomically before reuse.

use std::collections::{BTreeMap, BTreeSet};

use pg_execution::{VenueOrderSnapshot, VenueOrderState};
use rust_decimal::Decimal;

use crate::trade_history::{OwnedOrder, TradePage, UmTrade};

const MAX_PAGES: usize = 32;
const MAX_TRADES: usize = 32_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryGateError {
    MissingAnchor,
    IncompletePagination,
    InvalidCursor,
    DuplicateTrade,
    UnownedTrade,
    ConflictingOrder,
    FillMismatch,
    ResourceLimit,
}

#[derive(Debug, Clone)]
pub struct VerifiedTrade {
    pub durable_client_id: String,
    pub trade: UmTrade,
}

/// Evidence only. `next_from_id` must NOT be persisted until the entire batch
/// has been atomically written to the fill ledger and verified against OMS.
#[derive(Debug, Clone)]
pub struct VerifiedHistory {
    pub anchored_from_id: u64,
    pub next_from_id: u64,
    pub trades: Vec<VerifiedTrade>,
}

/// An order in this slice must originate from durable, independently checked
/// strategy ownership, not a matching `pg` prefix in the venue response.
#[derive(Debug, Clone)]
pub struct OwnedSnapshot<'a> {
    pub owner: OwnedOrder<'a>,
    pub snapshot: &'a VenueOrderSnapshot,
}

/// Validate complete bounded forward pagination from a trusted anchor.
/// A short final page only proves the interval *after* the supplied anchor;
/// it does not prove the anchor includes trades before the lookback window.
/// Every trade in an isolated account must have a matching owned order.
pub fn verify_trade_history(
    anchor: u64,
    pages: &[TradePage],
    orders: &[OwnedSnapshot<'_>],
) -> Result<VerifiedHistory, HistoryGateError> {
    if anchor == 0 {
        return Err(HistoryGateError::MissingAnchor);
    }
    if pages.is_empty() || !pages.last().is_some_and(|page| page.short_page) {
        return Err(HistoryGateError::IncompletePagination);
    }
    if pages.len() > MAX_PAGES || orders.len() > MAX_TRADES {
        return Err(HistoryGateError::ResourceLimit);
    }

    let mut by_venue_id = BTreeMap::new();
    let mut requested = BTreeMap::new();
    let mut cumulative = BTreeMap::new();
    for owned in orders {
        let snapshot = owned.snapshot;
        if owned.owner.venue_order_id != snapshot.venue_order_id
            || owned.owner.symbol != snapshot.asset
            || owned.owner.side != snapshot.side
            || snapshot.client_order_id.as_deref() != Some(owned.owner.client_order_id)
            || snapshot.venue_order_id.is_empty()
            || snapshot.requested_quantity <= Decimal::ZERO
            || snapshot.filled_quantity < Decimal::ZERO
            || snapshot.filled_quantity > snapshot.requested_quantity
            || matches!(snapshot.state, VenueOrderState::Unknown)
            || (matches!(snapshot.state, VenueOrderState::Filled)
                && snapshot.filled_quantity != snapshot.requested_quantity)
            || (matches!(snapshot.state, VenueOrderState::PartiallyFilled)
                && (snapshot.filled_quantity <= Decimal::ZERO
                    || snapshot.filled_quantity >= snapshot.requested_quantity))
        {
            return Err(HistoryGateError::ConflictingOrder);
        }
        if by_venue_id
            .insert(snapshot.venue_order_id.as_str(), &owned.owner)
            .is_some()
            || requested
                .insert(owned.owner.client_order_id, snapshot.filled_quantity)
                .is_some()
        {
            return Err(HistoryGateError::ConflictingOrder);
        }
        cumulative.insert(owned.owner.client_order_id, Decimal::ZERO);
    }

    let mut cursor = anchor;
    let mut seen = BTreeSet::new();
    let mut verified = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        if index + 1 != pages.len() && (page.short_page || page.trades.is_empty()) {
            return Err(HistoryGateError::IncompletePagination);
        }
        if page.trades.len() > 1000 || verified.len() + page.trades.len() > MAX_TRADES {
            return Err(HistoryGateError::ResourceLimit);
        }
        let mut last_id = None;
        for trade in &page.trades {
            if trade.trade_id < cursor
                || last_id.is_some_and(|previous| trade.trade_id <= previous)
                || !seen.insert(trade.trade_id)
            {
                return Err(HistoryGateError::DuplicateTrade);
            }
            let owner = by_venue_id
                .get(trade.venue_order_id.as_str())
                .ok_or(HistoryGateError::UnownedTrade)?;
            let durable_id = trade
                .verified_durable_order(owner)
                .map_err(|_| HistoryGateError::UnownedTrade)?;
            let quantity = cumulative
                .get_mut(durable_id)
                .ok_or(HistoryGateError::ConflictingOrder)?;
            *quantity = quantity
                .checked_add(trade.quantity)
                .ok_or(HistoryGateError::FillMismatch)?;
            if *quantity > requested[durable_id] {
                return Err(HistoryGateError::FillMismatch);
            }
            verified.push(VerifiedTrade {
                durable_client_id: durable_id.to_owned(),
                trade: trade.clone(),
            });
            last_id = Some(trade.trade_id);
        }
        let expected_next = match last_id {
            Some(last) => last.checked_add(1).ok_or(HistoryGateError::InvalidCursor)?,
            None => cursor,
        };
        if page.next_from_id != Some(expected_next) {
            return Err(HistoryGateError::InvalidCursor);
        }
        cursor = expected_next;
    }
    for (id, expected) in requested {
        if cumulative[&id] != expected {
            return Err(HistoryGateError::FillMismatch);
        }
    }
    Ok(VerifiedHistory {
        anchored_from_id: anchor,
        next_from_id: cursor,
        trades: verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{Side, Venue};

    const CLIENT: &str = "pg42424242424242424242424242424242";

    fn snapshot(filled: i64) -> VenueOrderSnapshot {
        VenueOrderSnapshot {
            venue_order_id: "123".into(),
            client_order_id: Some(CLIENT.into()),
            asset: "BTCUSDC".into(),
            side: Side::Buy,
            requested_quantity: Decimal::new(10, 3),
            filled_quantity: Decimal::new(filled, 3),
            limit_price: None,
            state: VenueOrderState::PartiallyFilled,
        }
    }

    fn trade(id: u64, qty: i64) -> UmTrade {
        UmTrade {
            symbol: "BTCUSDC".into(),
            trade_id: id,
            venue_order_id: "123".into(),
            side: Side::Buy,
            quantity: Decimal::new(qty, 3),
            price: Decimal::from(80_000),
            quote_quantity: Decimal::from(400),
            commission: Decimal::new(1, 3),
            commission_asset: "USDC".into(),
            realized_pnl: Decimal::ZERO,
            trade_time_ms: 1_700_000_000_000,
        }
    }

    fn owner<'a>(snapshot: &'a VenueOrderSnapshot) -> Vec<OwnedSnapshot<'a>> {
        vec![OwnedSnapshot {
            owner: OwnedOrder {
                venue: Venue::BinancePm,
                symbol: "BTCUSDC",
                client_order_id: CLIENT,
                venue_order_id: "123",
                side: Side::Buy,
            },
            snapshot,
        }]
    }

    fn page(trades: Vec<UmTrade>, next: u64, short: bool) -> TradePage {
        TradePage {
            trades,
            next_from_id: Some(next),
            short_page: short,
        }
    }

    #[test]
    fn exact_partial_fills_and_fee_evidence_verify_without_authorizing_orders() {
        let snapshot = snapshot(5);
        let evidence = verify_trade_history(
            9,
            &[page(vec![trade(9, 2), trade(12, 3)], 13, true)],
            &owner(&snapshot),
        )
        .unwrap();
        assert_eq!(evidence.next_from_id, 13);
        assert_eq!(evidence.trades.len(), 2);
        assert_eq!(evidence.trades[0].durable_client_id, CLIENT);
        assert_eq!(evidence.trades[1].trade.commission, Decimal::new(1, 3));
    }

    #[test]
    fn gaps_in_trade_ids_are_legal_but_missing_pages_and_bad_cursor_are_not() {
        let snapshot = snapshot(5);
        let orders = owner(&snapshot);
        assert_eq!(
            verify_trade_history(9, &[page(vec![trade(9, 5)], 10, false)], &orders)
                .unwrap_err(),
            HistoryGateError::IncompletePagination
        );
        assert_eq!(
            verify_trade_history(9, &[page(vec![trade(9, 5)], 11, true)], &orders)
                .unwrap_err(),
            HistoryGateError::InvalidCursor
        );
        assert_eq!(
            verify_trade_history(0, &[page(vec![trade(9, 5)], 10, true)], &orders)
                .unwrap_err(),
            HistoryGateError::MissingAnchor
        );
    }

    #[test]
    fn duplicate_foreign_and_unexplained_cumulative_fills_fail_closed() {
        let snapshot = snapshot(5);
        let orders = owner(&snapshot);
        assert_eq!(
            verify_trade_history(
                9,
                &[page(vec![trade(9, 2), trade(9, 3)], 10, true)],
                &orders,
            )
            .unwrap_err(),
            HistoryGateError::DuplicateTrade
        );
        let mut foreign = trade(9, 5);
        foreign.venue_order_id = "999".into();
        assert_eq!(
            verify_trade_history(9, &[page(vec![foreign], 10, true)], &orders).unwrap_err(),
            HistoryGateError::UnownedTrade
        );
        assert_eq!(
            verify_trade_history(9, &[page(vec![trade(9, 4)], 10, true)], &orders)
                .unwrap_err(),
            HistoryGateError::FillMismatch
        );
    }

    #[test]
    fn contradictory_order_or_inconsistent_page_never_passes() {
        let mut snapshot = snapshot(5);
        snapshot.state = VenueOrderState::Filled;
        assert_eq!(
            verify_trade_history(9, &[page(vec![trade(9, 5)], 10, true)], &owner(&snapshot))
                .unwrap_err(),
            HistoryGateError::ConflictingOrder
        );
        let snapshot = snapshot(5);
        assert_eq!(
            verify_trade_history(
                9,
                &[
                    page(vec![trade(9, 2)], 10, true),
                    page(vec![trade(10, 3)], 11, true),
                ],
                &owner(&snapshot),
            )
            .unwrap_err(),
            HistoryGateError::IncompletePagination
        );
    }
}
