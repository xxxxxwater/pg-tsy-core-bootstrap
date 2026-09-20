use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimSide { Buy, Sell }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind { Market, Limit }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce { Ioc, Fok, Gtc, Gtd, Day, AtTheOpen, AtTheClose }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketPhase { PreOpen, Opening, Continuous, Closing, Closed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContingencyKind { Oco, Oto, Ouo }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contingency {
    pub group_id: Uuid,
    pub kind: ContingencyKind,
    pub peer_order_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimOrder {
    pub order_id: Uuid,
    pub side: SimSide,
    pub kind: OrderKind,
    pub quantity: Decimal,
    pub limit_price: Option<Decimal>,
    pub time_in_force: TimeInForce,
    pub expire_at_ns: Option<u64>,
    pub post_only: bool,
    pub reduce_only: bool,
    pub display_quantity: Option<Decimal>,
    pub contingency: Option<Contingency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimOrderState { Dormant, Resting, PartiallyFilled, Filled, Canceled, Rejected }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimOrderRecord {
    pub order: SimOrder,
    pub state: SimOrderState,
    pub filled_quantity: Decimal,
}

impl SimOrderRecord {
    pub fn remaining(&self) -> Decimal {
        (self.order.quantity - self.filled_quantity).max(Decimal::ZERO)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MarketSnapshot {
    pub bid_price: Decimal,
    pub bid_quantity: Decimal,
    pub ask_price: Decimal,
    pub ask_quantity: Decimal,
    pub phase: MarketPhase,
    pub now_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub order_id: Uuid,
    pub quantity: Decimal,
    pub price: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    Filled(Fill),
    PartiallyFilled(Fill),
    Resting,
    Canceled,
    Rejected(&'static str),
    Dormant,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SimError {
    #[error("quantity must be positive")]
    InvalidQuantity,
    #[error("limit orders require a positive limit price")]
    InvalidLimitPrice,
    #[error("GTD orders require expire_at_ns")]
    MissingExpiry,
    #[error("iceberg display quantity must be positive and no greater than total quantity")]
    InvalidDisplayQuantity,
    #[error("order {0} already exists")]
    DuplicateOrder(Uuid),
    #[error("order {0} was not found")]
    UnknownOrder(Uuid),
}

#[derive(Debug, Default)]
pub struct MatchingEngine {
    orders: BTreeMap<Uuid, SimOrderRecord>,
}

impl MatchingEngine {
    pub fn submit(&mut self, order: SimOrder, active: bool) -> Result<(), SimError> {
        validate_order(&order)?;
        if self.orders.contains_key(&order.order_id) {
            return Err(SimError::DuplicateOrder(order.order_id));
        }
        self.orders.insert(order.order_id, SimOrderRecord {
            order,
            state: if active { SimOrderState::Resting } else { SimOrderState::Dormant },
            filled_quantity: Decimal::ZERO,
        });
        Ok(())
    }

    pub fn record(&self, order_id: Uuid) -> Option<&SimOrderRecord> {
        self.orders.get(&order_id)
    }

    pub fn cancel(&mut self, order_id: Uuid) -> Result<(), SimError> {
        let record = self.orders.get_mut(&order_id).ok_or(SimError::UnknownOrder(order_id))?;
        if !matches!(record.state, SimOrderState::Filled | SimOrderState::Canceled | SimOrderState::Rejected) {
            record.state = SimOrderState::Canceled;
        }
        Ok(())
    }

    pub fn match_once(
        &mut self,
        order_id: Uuid,
        market: MarketSnapshot,
        signed_position: Decimal,
    ) -> Result<MatchOutcome, SimError> {
        let mut activate_peer = None;
        let mut cancel_peer = None;
        let mut resize_peer = None;

        let outcome = {
            let record = self.orders.get_mut(&order_id).ok_or(SimError::UnknownOrder(order_id))?;

            if record.state == SimOrderState::Dormant {
                return Ok(MatchOutcome::Dormant);
            }
            if matches!(record.state, SimOrderState::Filled | SimOrderState::Canceled | SimOrderState::Rejected) {
                return Ok(match record.state {
                    SimOrderState::Canceled => MatchOutcome::Canceled,
                    SimOrderState::Rejected => MatchOutcome::Rejected("already rejected"),
                    _ => MatchOutcome::Resting,
                });
            }
            if is_expired(&record.order, market) {
                record.state = SimOrderState::Canceled;
                return Ok(MatchOutcome::Canceled);
            }
            if !phase_allows(&record.order, market.phase) {
                return Ok(MatchOutcome::Resting);
            }

            let (touch_price, available) = match record.order.side {
                SimSide::Buy => (market.ask_price, market.ask_quantity),
                SimSide::Sell => (market.bid_price, market.bid_quantity),
            };
            let crosses = match record.order.kind {
                OrderKind::Market => true,
                OrderKind::Limit => match (record.order.side, record.order.limit_price) {
                    (SimSide::Buy, Some(px)) => px >= touch_price,
                    (SimSide::Sell, Some(px)) => px <= touch_price,
                    _ => false,
                },
            };
            if record.order.post_only && crosses {
                record.state = SimOrderState::Rejected;
                return Ok(MatchOutcome::Rejected("post-only order would take liquidity"));
            }
            if !crosses {
                return Ok(MatchOutcome::Resting);
            }

            let remaining = record.remaining();
            let reducible = if record.order.reduce_only {
                match record.order.side {
                    SimSide::Buy if signed_position < Decimal::ZERO => -signed_position,
                    SimSide::Sell if signed_position > Decimal::ZERO => signed_position,
                    _ => Decimal::ZERO,
                }
            } else {
                remaining
            };
            if record.order.reduce_only && reducible <= Decimal::ZERO {
                record.state = SimOrderState::Rejected;
                return Ok(MatchOutcome::Rejected("reduce-only order would not reduce current position"));
            }

            let displayed = record.order.display_quantity.unwrap_or(remaining).min(remaining);
            let executable = remaining.min(available).min(displayed).min(reducible);

            if record.order.time_in_force == TimeInForce::Fok && executable < remaining {
                record.state = SimOrderState::Canceled;
                return Ok(MatchOutcome::Canceled);
            }
            if executable <= Decimal::ZERO {
                if record.order.time_in_force == TimeInForce::Ioc {
                    record.state = SimOrderState::Canceled;
                    return Ok(MatchOutcome::Canceled);
                }
                return Ok(MatchOutcome::Resting);
            }

            record.filled_quantity += executable;
            let fill = Fill { order_id, quantity: executable, price: touch_price };
            let fully_filled = record.remaining() == Decimal::ZERO;
            if fully_filled {
                record.state = SimOrderState::Filled;
            } else if record.order.time_in_force == TimeInForce::Ioc {
                record.state = SimOrderState::Canceled;
            } else {
                record.state = SimOrderState::PartiallyFilled;
            }

            if let Some(link) = &record.order.contingency {
                match link.kind {
                    ContingencyKind::Oco if fully_filled => cancel_peer = Some(link.peer_order_id),
                    ContingencyKind::Oto if fully_filled => activate_peer = Some(link.peer_order_id),
                    ContingencyKind::Ouo => resize_peer = Some((link.peer_order_id, executable)),
                    _ => {}
                }
            }

            if fully_filled { MatchOutcome::Filled(fill) } else { MatchOutcome::PartiallyFilled(fill) }
        };

        if let Some(peer) = cancel_peer {
            let _ = self.cancel(peer);
        }
        if let Some(peer) = activate_peer {
            if let Some(record) = self.orders.get_mut(&peer) {
                if record.state == SimOrderState::Dormant {
                    record.state = SimOrderState::Resting;
                }
            }
        }
        if let Some((peer, delta)) = resize_peer {
            if let Some(record) = self.orders.get_mut(&peer) {
                if !matches!(record.state, SimOrderState::Filled | SimOrderState::Canceled | SimOrderState::Rejected) {
                    record.order.quantity = (record.order.quantity - delta).max(record.filled_quantity);
                    if record.remaining() == Decimal::ZERO {
                        record.state = SimOrderState::Canceled;
                    }
                }
            }
        }

        Ok(outcome)
    }
}

fn validate_order(order: &SimOrder) -> Result<(), SimError> {
    if order.quantity <= Decimal::ZERO {
        return Err(SimError::InvalidQuantity);
    }
    if order.kind == OrderKind::Limit {
        match order.limit_price {
            Some(px) if px > Decimal::ZERO => {}
            _ => return Err(SimError::InvalidLimitPrice),
        }
    }
    if order.time_in_force == TimeInForce::Gtd && order.expire_at_ns.is_none() {
        return Err(SimError::MissingExpiry);
    }
    if let Some(display) = order.display_quantity {
        if display <= Decimal::ZERO || display > order.quantity {
            return Err(SimError::InvalidDisplayQuantity);
        }
    }
    Ok(())
}

fn is_expired(order: &SimOrder, market: MarketSnapshot) -> bool {
    match order.time_in_force {
        TimeInForce::Gtd => order.expire_at_ns.is_some_and(|deadline| market.now_ns >= deadline),
        TimeInForce::Day => market.phase == MarketPhase::Closed,
        _ => false,
    }
}

fn phase_allows(order: &SimOrder, phase: MarketPhase) -> bool {
    match order.time_in_force {
        TimeInForce::AtTheOpen => phase == MarketPhase::Opening,
        TimeInForce::AtTheClose => phase == MarketPhase::Closing,
        _ => !matches!(phase, MarketPhase::PreOpen | MarketPhase::Closed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: i64) -> Decimal { Decimal::from(v) }

    fn market(phase: MarketPhase) -> MarketSnapshot {
        MarketSnapshot {
            bid_price: d(99), bid_quantity: d(5),
            ask_price: d(100), ask_quantity: d(5),
            phase, now_ns: 100,
        }
    }

    fn limit(side: SimSide, qty: i64, px: i64, tif: TimeInForce) -> SimOrder {
        SimOrder {
            order_id: Uuid::new_v4(),
            side,
            kind: OrderKind::Limit,
            quantity: d(qty),
            limit_price: Some(d(px)),
            time_in_force: tif,
            expire_at_ns: None,
            post_only: false,
            reduce_only: false,
            display_quantity: None,
            contingency: None,
        }
    }

    #[test]
    fn ioc_fills_available_and_cancels_remainder() {
        let mut engine = MatchingEngine::default();
        let order = limit(SimSide::Buy, 10, 101, TimeInForce::Ioc);
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        let out = engine.match_once(id, market(MarketPhase::Continuous), d(0)).unwrap();
        assert!(matches!(out, MatchOutcome::PartiallyFilled(_)));
        let record = engine.record(id).unwrap();
        assert_eq!(record.filled_quantity, d(5));
        assert_eq!(record.state, SimOrderState::Canceled);
    }

    #[test]
    fn fok_is_all_or_none() {
        let mut engine = MatchingEngine::default();
        let order = limit(SimSide::Buy, 10, 101, TimeInForce::Fok);
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        assert_eq!(engine.match_once(id, market(MarketPhase::Continuous), d(0)).unwrap(), MatchOutcome::Canceled);
        assert_eq!(engine.record(id).unwrap().filled_quantity, Decimal::ZERO);
    }

    #[test]
    fn post_only_rejects_crossing_order() {
        let mut engine = MatchingEngine::default();
        let mut order = limit(SimSide::Buy, 1, 100, TimeInForce::Gtc);
        order.post_only = true;
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        assert!(matches!(engine.match_once(id, market(MarketPhase::Continuous), d(0)).unwrap(), MatchOutcome::Rejected(_)));
    }

    #[test]
    fn reduce_only_never_flips_position() {
        let mut engine = MatchingEngine::default();
        let mut order = limit(SimSide::Sell, 10, 99, TimeInForce::Gtc);
        order.reduce_only = true;
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        let out = engine.match_once(id, market(MarketPhase::Continuous), d(3)).unwrap();
        assert!(matches!(out, MatchOutcome::Filled(Fill { quantity, .. }) if quantity == d(3)));
        assert_eq!(engine.record(id).unwrap().filled_quantity, d(3));
    }

    #[test]
    fn iceberg_limits_visible_slice_per_match_step() {
        let mut engine = MatchingEngine::default();
        let mut order = limit(SimSide::Buy, 10, 101, TimeInForce::Gtc);
        order.display_quantity = Some(d(2));
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        let out = engine.match_once(id, market(MarketPhase::Continuous), d(0)).unwrap();
        assert!(matches!(out, MatchOutcome::PartiallyFilled(Fill { quantity, .. }) if quantity == d(2)));
    }

    #[test]
    fn open_and_close_orders_are_phase_gated() {
        let mut engine = MatchingEngine::default();
        let order = limit(SimSide::Buy, 1, 101, TimeInForce::AtTheOpen);
        let id = order.order_id;
        engine.submit(order, true).unwrap();
        assert_eq!(engine.match_once(id, market(MarketPhase::Continuous), d(0)).unwrap(), MatchOutcome::Resting);
        assert!(matches!(engine.match_once(id, market(MarketPhase::Opening), d(0)).unwrap(), MatchOutcome::Filled(_)));
    }

    #[test]
    fn oco_fill_cancels_peer() {
        let mut engine = MatchingEngine::default();
        let mut a = limit(SimSide::Buy, 1, 101, TimeInForce::Gtc);
        let b = limit(SimSide::Sell, 1, 98, TimeInForce::Gtc);
        let b_id = b.order_id;
        a.contingency = Some(Contingency { group_id: Uuid::new_v4(), kind: ContingencyKind::Oco, peer_order_id: b_id });
        let a_id = a.order_id;
        engine.submit(a, true).unwrap();
        engine.submit(b, true).unwrap();
        engine.match_once(a_id, market(MarketPhase::Continuous), d(0)).unwrap();
        assert_eq!(engine.record(b_id).unwrap().state, SimOrderState::Canceled);
    }

    #[test]
    fn oto_fill_activates_dormant_peer() {
        let mut engine = MatchingEngine::default();
        let child = limit(SimSide::Sell, 1, 98, TimeInForce::Gtc);
        let child_id = child.order_id;
        let mut parent = limit(SimSide::Buy, 1, 101, TimeInForce::Gtc);
        parent.contingency = Some(Contingency { group_id: Uuid::new_v4(), kind: ContingencyKind::Oto, peer_order_id: child_id });
        let parent_id = parent.order_id;
        engine.submit(parent, true).unwrap();
        engine.submit(child, false).unwrap();
        engine.match_once(parent_id, market(MarketPhase::Continuous), d(0)).unwrap();
        assert_eq!(engine.record(child_id).unwrap().state, SimOrderState::Resting);
    }

    #[test]
    fn ouo_fill_reduces_peer_quantity() {
        let mut engine = MatchingEngine::default();
        let peer = limit(SimSide::Sell, 10, 98, TimeInForce::Gtc);
        let peer_id = peer.order_id;
        let mut primary = limit(SimSide::Buy, 10, 101, TimeInForce::Gtc);
        primary.display_quantity = Some(d(2));
        primary.contingency = Some(Contingency { group_id: Uuid::new_v4(), kind: ContingencyKind::Ouo, peer_order_id: peer_id });
        let primary_id = primary.order_id;
        engine.submit(primary, true).unwrap();
        engine.submit(peer, true).unwrap();
        engine.match_once(primary_id, market(MarketPhase::Continuous), d(0)).unwrap();
        assert_eq!(engine.record(peer_id).unwrap().order.quantity, d(8));
    }
}
