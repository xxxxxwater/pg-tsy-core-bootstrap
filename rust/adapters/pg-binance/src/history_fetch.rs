//! Bounded signed UM REST history collection for isolated read-only reconciliation.
//!
//! This fetcher DOES NOT establish the historical genesis anchor, prove that
//! caller-supplied order snapshots are account-wide, persist a cursor, update OMS
//! or authorize a new order. The caller must acquire authenticated snapshots
//! independently, supply an externally verified anchor, then atomically settle
//! the returned evidence and its cursor before permitting exposure.

use async_trait::async_trait;
use pg_execution::ExecutionError;

use crate::{
    history_reconcile::{OwnedSnapshot, VerifiedHistory, verify_trade_history},
    rest_transport::BinanceRestClient,
    trade_history::TradePage,
};

const PAGE_LIMIT: u16 = 1000;
const MAX_PAGES: usize = 32;

/// Allows deterministic failure injection without any account credentials.
#[async_trait]
pub trait SignedTradePageReader: Send + Sync {
    async fn read_page(&self, from_id: u64, limit: u16) -> Result<TradePage, ExecutionError>;
}

#[async_trait]
impl SignedTradePageReader for BinanceRestClient {
    async fn read_page(&self, from_id: u64, limit: u16) -> Result<TradePage, ExecutionError> {
        self.user_trades_page(Some(from_id), limit).await
    }
}

/// Collect only a bounded, forward-moving interval. The final short page is
/// mandatory; exhaustion, duplicates, unknown orders, bad snapshots and gaps
/// in cumulative quantity are fatal. A successful read is evidence, NOT an
/// authorization to advance the durable cursor or lift SAFE_HOLD.
pub async fn fetch_verified_history<R: SignedTradePageReader>(
    reader: &R,
    externally_verified_anchor: u64,
    authenticated_owned_orders: &[OwnedSnapshot<'_>],
) -> Result<VerifiedHistory, ExecutionError> {
    if externally_verified_anchor == 0 || externally_verified_anchor > i64::MAX as u64 {
        return Err(ExecutionError::Conversion(
            "missing or invalid verified history anchor".into(),
        ));
    }
    let mut cursor = externally_verified_anchor;
    let mut pages = Vec::new();
    for _ in 0..MAX_PAGES {
        // Each network read is independently signed by BinanceRestClient.
        // There is no retry after an ambiguous or malformed history page.
        let page = reader.read_page(cursor, PAGE_LIMIT).await?;
        let next = page
            .next_from_id
            .ok_or_else(|| ExecutionError::Conversion("UM history page lacks a cursor".into()))?;
        if next < cursor || next > i64::MAX as u64 || (!page.short_page && next == cursor) {
            return Err(ExecutionError::Conversion(
                "UM history cursor failed to advance".into(),
            ));
        }
        let terminal = page.short_page;
        pages.push(page);
        if terminal {
            return verify_trade_history(
                externally_verified_anchor,
                &pages,
                authenticated_owned_orders,
            )
            .map_err(|_| {
                ExecutionError::Conversion(
                    "UM history ownership or fill verification failed".into(),
                )
            });
        }
        cursor = next;
    }
    Err(ExecutionError::Conversion(
        "UM history exceeded bounded pagination; SAFE_HOLD".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trade_history::{OwnedOrder, UmTrade};
    use pg_execution::{VenueOrderSnapshot, VenueOrderState};
    use pg_types::{Side, Venue};
    use rust_decimal::Decimal;
    use std::sync::Mutex;

    const CLIENT: &str = "pg42424242424242424242424242424242";

    struct Reader {
        pages: Mutex<Vec<TradePage>>,
        cursors: Mutex<Vec<u64>>,
        fail: bool,
    }

    #[async_trait]
    impl SignedTradePageReader for Reader {
        async fn read_page(&self, from_id: u64, limit: u16) -> Result<TradePage, ExecutionError> {
            assert_eq!(limit, PAGE_LIMIT);
            self.cursors.lock().unwrap().push(from_id);
            if self.fail {
                return Err(ExecutionError::Transport(
                    "injected signed REST outage".into(),
                ));
            }
            let mut pages = self.pages.lock().unwrap();
            if pages.is_empty() {
                return Err(ExecutionError::Transport(
                    "unexpected extra REST read".into(),
                ));
            }
            Ok(pages.remove(0))
        }
    }

    fn fixture() -> (VenueOrderSnapshot, Vec<TradePage>) {
        let snapshot = VenueOrderSnapshot {
            venue_order_id: "123".into(),
            client_order_id: Some(CLIENT.into()),
            asset: "BTCUSDC".into(),
            side: Side::Buy,
            requested_quantity: Decimal::new(10, 3),
            filled_quantity: Decimal::new(5, 3),
            limit_price: None,
            state: VenueOrderState::PartiallyFilled,
        };
        let trade = UmTrade {
            symbol: "BTCUSDC".into(),
            trade_id: 9,
            venue_order_id: "123".into(),
            side: Side::Buy,
            quantity: Decimal::new(5, 3),
            price: Decimal::from(80_000),
            quote_quantity: Decimal::from(400),
            commission: Decimal::new(1, 3),
            commission_asset: "USDC".into(),
            realized_pnl: Decimal::ZERO,
            trade_time_ms: 1_700_000_000_000,
        };
        (
            snapshot,
            vec![TradePage {
                trades: vec![trade],
                next_from_id: Some(10),
                short_page: true,
            }],
        )
    }

    fn owned(snapshot: &VenueOrderSnapshot) -> Vec<OwnedSnapshot<'_>> {
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

    #[tokio::test]
    async fn signed_read_yields_evidence_but_never_modifies_orders() {
        let (snapshot, pages) = fixture();
        let reader = Reader {
            pages: Mutex::new(pages),
            cursors: Mutex::new(Vec::new()),
            fail: false,
        };
        let result = fetch_verified_history(&reader, 9, &owned(&snapshot))
            .await
            .unwrap();
        assert_eq!(result.anchored_from_id, 9);
        assert_eq!(result.next_from_id, 10);
        assert_eq!(result.trades[0].durable_client_id, CLIENT);
        assert_eq!(*reader.cursors.lock().unwrap(), vec![9]);
    }

    #[tokio::test]
    async fn missing_anchor_unauthenticated_order_and_failed_rest_are_blocked() {
        let (snapshot, pages) = fixture();
        let reader = Reader {
            pages: Mutex::new(pages),
            cursors: Mutex::new(Vec::new()),
            fail: false,
        };
        assert!(
            fetch_verified_history(&reader, 0, &owned(&snapshot))
                .await
                .is_err()
        );
        assert!(reader.cursors.lock().unwrap().is_empty());
        assert!(fetch_verified_history(&reader, 9, &[]).await.is_err());
        let reader = Reader {
            pages: Mutex::new(Vec::new()),
            cursors: Mutex::new(Vec::new()),
            fail: true,
        };
        assert!(
            fetch_verified_history(&reader, 9, &owned(&snapshot))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn full_page_without_next_progress_never_loops_or_advances() {
        let (snapshot, _) = fixture();
        let reader = Reader {
            pages: Mutex::new(vec![TradePage {
                trades: vec![],
                next_from_id: Some(9),
                short_page: false,
            }]),
            cursors: Mutex::new(Vec::new()),
            fail: false,
        };
        assert!(
            fetch_verified_history(&reader, 9, &owned(&snapshot))
                .await
                .is_err()
        );
        assert_eq!(*reader.cursors.lock().unwrap(), vec![9]);
    }
}
