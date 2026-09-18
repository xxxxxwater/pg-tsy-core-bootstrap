//! Portfolio Margin user-stream boundary: no event can authorize a new order.
//! An unknown event, disconnect, gap or inconsistent fill forces REST reconcile.
//! WS events are advisory until ownership is matched against a durable intent.

use std::collections::{BTreeMap, BTreeSet};

use pg_execution::VenueOrderSnapshot;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::{
    durable_client_order_id,
    order_protocol::{RawOrder, SYMBOL, normalize_order},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    Malformed,
    UnknownEvent,
    InvalidOrder,
    ReconcileRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillNotice {
    pub trade_id: u64,
    pub venue_order_id: String,
    pub durable_client_id: String,
    pub quantity: Decimal,
    pub price: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub trade_time_ms: u64,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    /// The caller must still confirm `durable_client_id` belongs to this
    /// strategy/lease in persistent storage before mutating its order record.
    Order {
        durable_client_id: String,
        snapshot: VenueOrderSnapshot,
        fill: Box<Option<FillNotice>>,
    },
    /// ACCOUNT_UPDATE, ALGO_UPDATE, listenKeyExpired, reconnect and foreign
    /// orders are not permission to change a strategy's owned positions.
    ReconcileRequired,
}

fn string<'a>(object: &'a Value, key: &str) -> Result<&'a str, StreamError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(StreamError::Malformed)
}

fn unsigned(object: &Value, key: &str) -> Result<u64, StreamError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(StreamError::Malformed)
}

fn nonnegative_decimal(object: &Value, key: &str) -> Result<Decimal, StreamError> {
    let value = string(object, key)?
        .parse::<Decimal>()
        .map_err(|_| StreamError::Malformed)?;
    if value < Decimal::ZERO {
        return Err(StreamError::Malformed);
    }
    Ok(value)
}

/// Parse one `ORDER_TRADE_UPDATE` using its actual nested `o` payload.
/// Never interpret unknown event types as a successful order or an empty fill.
pub fn decode_user_event(bytes: &[u8]) -> Result<UserEvent, StreamError> {
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        return Err(StreamError::Malformed);
    }
    let message: Value = serde_json::from_slice(bytes).map_err(|_| StreamError::Malformed)?;
    match string(&message, "e")? {
        "ACCOUNT_UPDATE" | "ALGO_UPDATE" | "listenKeyExpired" | "MARGIN_CALL" => {
            return Ok(UserEvent::ReconcileRequired);
        }
        "ORDER_TRADE_UPDATE" => {}
        _ => return Err(StreamError::UnknownEvent),
    }
    let order = message.get("o").ok_or(StreamError::Malformed)?;
    if string(order, "s")? != SYMBOL {
        return Ok(UserEvent::ReconcileRequired);
    }
    let venue_id = string(order, "c")?;
    let Some(durable_id) = durable_client_order_id(venue_id) else {
        return Ok(UserEvent::ReconcileRequired);
    };
    if string(order, "ps")? != "BOTH" {
        return Ok(UserEvent::ReconcileRequired);
    }
    if unsigned(&message, "E")? == 0 || unsigned(&message, "T")? == 0 {
        return Err(StreamError::Malformed);
    }
    let order_id = unsigned(order, "i")?.to_string();
    let mut snapshot = normalize_order(
        RawOrder {
            symbol: SYMBOL,
            client_order_id: venue_id,
            order_id: &order_id,
            side: string(order, "S")?,
            original_quantity: string(order, "q")?,
            executed_quantity: string(order, "z")?,
            price: string(order, "p")?,
            status: string(order, "X")?,
        },
        venue_id,
    )
    .map_err(|_| StreamError::InvalidOrder)?;
    snapshot.client_order_id = Some(durable_id.clone());
    let fill = if string(order, "x")? == "TRADE" {
        let quantity = nonnegative_decimal(order, "l")?;
        let price = nonnegative_decimal(order, "L")?;
        let commission = nonnegative_decimal(order, "n")?;
        let trade_id = unsigned(order, "t")?;
        let time = unsigned(order, "T")?;
        if quantity <= Decimal::ZERO
            || price <= Decimal::ZERO
            || time == 0
            || quantity > snapshot.filled_quantity
        {
            return Err(StreamError::InvalidOrder);
        }
        Some(FillNotice {
            trade_id,
            venue_order_id: order_id,
            durable_client_id: durable_id.clone(),
            quantity,
            price,
            commission,
            commission_asset: string(order, "N")?.to_owned(),
            trade_time_ms: time,
        })
    } else {
        // Unknown execution types demand reconciliation rather than assuming
        // that a positive executed quantity means an incoming new fill.
        match string(order, "x")? {
            "NEW" | "CANCELED" | "EXPIRED" | "REJECTED" | "AMENDMENT" | "CALCULATED" => None,
            _ => return Err(StreamError::UnknownEvent),
        }
    };
    Ok(UserEvent::Order {
        durable_client_id: durable_id,
        snapshot,
        fill: Box::new(fill),
    })
}

/// In-memory dedup is only a secondary guard. Persisted order history and
/// exchange userTrades remain the source of truth after process restart.
#[derive(Default)]
pub struct UserOrderTracker {
    cumulative: BTreeMap<String, Decimal>,
    seen_trades: BTreeSet<(String, u64)>,
    healthy: bool,
}

impl UserOrderTracker {
    pub fn mark_reconciled(&mut self) {
        self.healthy = true;
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    pub fn disconnect(&mut self) {
        self.healthy = false;
        self.cumulative.clear();
        self.seen_trades.clear();
    }

    /// Returns whether the fill is new within this connection. Duplicate event
    /// delivery is harmless. Once an order has a previous snapshot, a positive
    /// cumulative change MUST have exactly one matching new TRADE fill. Missing,
    /// repeated or contradictory trade IDs require REST reconciliation; never
    /// synthesize missing fills from cumulative quantity alone.
    pub fn accept(&mut self, event: &UserEvent) -> Result<bool, StreamError> {
        if !self.healthy {
            return Err(StreamError::ReconcileRequired);
        }
        let UserEvent::Order {
            durable_client_id,
            snapshot,
            fill,
        } = event
        else {
            self.disconnect();
            return Err(StreamError::ReconcileRequired);
        };
        let previous = self.cumulative.get(durable_client_id).copied();
        let trade_key = fill
            .as_ref()
            .as_ref()
            .map(|trade| (durable_client_id.clone(), trade.trade_id));
        if let Some(previous) = previous {
            let delta = snapshot.filled_quantity - previous;
            let consistent = if delta < Decimal::ZERO {
                false
            } else if delta > Decimal::ZERO {
                fill.as_ref().as_ref().is_some_and(|trade| {
                    trade.quantity == delta
                        && trade_key
                            .as_ref()
                            .is_some_and(|key| !self.seen_trades.contains(key))
                })
            } else {
                trade_key
                    .as_ref()
                    .is_none_or(|key| self.seen_trades.contains(key))
            };
            if !consistent {
                self.disconnect();
                return Err(StreamError::ReconcileRequired);
            }
        }
        self.cumulative
            .insert(durable_client_id.clone(), snapshot.filled_quantity);
        Ok(trade_key.is_some_and(|key| self.seen_trades.insert(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_intent_bytes;

    fn trade() -> Vec<u8> {
        let venue_id = encode_intent_bytes(&[0x42; 16]);
        format!(r#"{{"e":"ORDER_TRADE_UPDATE","E":1700000000000,"T":1700000000000,"o":{{"s":"BTCUSDC","c":"{venue_id}","ps":"BOTH","i":123,"S":"BUY","q":"0.010","z":"0.005","p":"80000","X":"PARTIALLY_FILLED","x":"TRADE","l":"0.005","L":"80000","n":"0.001","N":"USDC","t":9,"T":1700000000000}}}}"#).into_bytes()
    }

    #[test]
    fn owned_partial_fill_maps_to_persisted_core_id() {
        let UserEvent::Order {
            durable_client_id,
            snapshot,
            fill,
        } = decode_user_event(&trade()).unwrap()
        else {
            panic!("expected order");
        };
        assert_eq!(durable_client_id, format!("pg{}", "42".repeat(16)));
        assert_eq!(
            snapshot.client_order_id.as_deref(),
            Some(durable_client_id.as_str())
        );
        assert_eq!(fill.unwrap().quantity.to_string(), "0.005");
    }

    #[test]
    fn stream_reconnect_requires_reconcile_and_deduplicates_trade() {
        let event = decode_user_event(&trade()).unwrap();
        let mut tracker = UserOrderTracker::default();
        assert_eq!(tracker.accept(&event), Err(StreamError::ReconcileRequired));
        tracker.mark_reconciled();
        assert_eq!(tracker.accept(&event), Ok(true));
        assert_eq!(tracker.accept(&event), Ok(false));
        tracker.disconnect();
        assert!(!tracker.is_healthy());
        assert_eq!(tracker.accept(&event), Err(StreamError::ReconcileRequired));
    }

    #[test]
    fn unexplained_cumulative_fill_requires_rest_reconciliation() {
        let baseline = decode_user_event(&trade()).unwrap();
        let mut tracker = UserOrderTracker::default();
        tracker.mark_reconciled();
        assert_eq!(tracker.accept(&baseline), Ok(true));
        let mut missing_trade = baseline.clone();
        if let UserEvent::Order { snapshot, fill, .. } = &mut missing_trade {
            snapshot.filled_quantity += Decimal::new(1, 3);
            **fill = None;
        }
        assert_eq!(tracker.accept(&missing_trade), Err(StreamError::ReconcileRequired));
        assert!(!tracker.is_healthy());
    }

    #[test]
    fn mismatched_or_replayed_trade_id_cannot_explain_new_quantity() {
        let baseline = decode_user_event(&trade()).unwrap();
        let mut tracker = UserOrderTracker::default();
        tracker.mark_reconciled();
        assert_eq!(tracker.accept(&baseline), Ok(true));
        let mut mismatched = baseline.clone();
        if let UserEvent::Order { snapshot, fill, .. } = &mut mismatched {
            snapshot.filled_quantity += Decimal::new(2, 3);
            fill.as_mut().as_mut().expect("trade").quantity = Decimal::new(1, 3);
        }
        assert_eq!(tracker.accept(&mismatched), Err(StreamError::ReconcileRequired));
        tracker.mark_reconciled();
        assert_eq!(tracker.accept(&baseline), Ok(true));
        let mut replayed = baseline.clone();
        if let UserEvent::Order { snapshot, .. } = &mut replayed {
            snapshot.filled_quantity += Decimal::new(5, 3);
        }
        assert_eq!(tracker.accept(&replayed), Err(StreamError::ReconcileRequired));
    }

    #[test]
    fn new_trade_id_without_cumulative_change_requires_reconcile() {
        let baseline = decode_user_event(&trade()).unwrap();
        let mut tracker = UserOrderTracker::default();
        tracker.mark_reconciled();
        assert_eq!(tracker.accept(&baseline), Ok(true));
        let mut contradictory = baseline.clone();
        if let UserEvent::Order { fill, .. } = &mut contradictory {
            fill.as_mut().as_mut().expect("trade").trade_id = 10;
        }
        assert_eq!(tracker.accept(&contradictory), Err(StreamError::ReconcileRequired));
    }

    #[test]
    fn unrelated_account_update_and_manual_orders_never_adopt() {
        assert!(matches!(
            decode_user_event(br#"{"e":"ACCOUNT_UPDATE"}"#),
            Ok(UserEvent::ReconcileRequired)
        ));
        let mut foreign = String::from_utf8(trade()).unwrap();
        foreign = foreign.replace(&encode_intent_bytes(&[0x42; 16]), "manual123");
        assert!(matches!(
            decode_user_event(foreign.as_bytes()),
            Ok(UserEvent::ReconcileRequired)
        ));
    }

    #[test]
    fn contradictory_fills_fail_closed() {
        let bad = String::from_utf8(trade())
            .unwrap()
            .replace("\"l\":\"0.005\"", "\"l\":\"0.020\"");
        assert!(matches!(
            decode_user_event(bad.as_bytes()),
            Err(StreamError::InvalidOrder)
        ));
        assert!(matches!(
            decode_user_event(br#"{"e":"unknown"}"#),
            Err(StreamError::UnknownEvent)
        ));
    }
}
