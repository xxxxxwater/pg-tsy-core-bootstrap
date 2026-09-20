use pg_types::{
    ExposureEffect, Side,
    advanced_order::{AdvancedOrderIntent, CompositeInstruction, TimeInForce},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionPhase {
    PreOpen,
    OpenAuction,
    Continuous,
    CloseAuction,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopOfBook {
    pub bid_price: Decimal,
    pub bid_quantity: Decimal,
    pub ask_price: Decimal,
    pub ask_quantity: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MatchDisposition {
    Filled,
    PartiallyFilledAndCanceled,
    Canceled,
    Resting,
    Rejected,
    Expired,
    WaitingForParent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResult {
    pub disposition: MatchDisposition,
    pub filled_quantity: Decimal,
    pub remaining_quantity: Decimal,
    pub fill_price: Option<Decimal>,
    pub visible_quantity: Decimal,
    pub reason: Option<String>,
}

impl MatchResult {
    fn reject(order: &AdvancedOrderIntent, reason: impl Into<String>) -> Self {
        Self {
            disposition: MatchDisposition::Rejected,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: order.base.quantity,
            fill_price: None,
            visible_quantity: order.visible_quantity(),
            reason: Some(reason.into()),
        }
    }

    fn expire(order: &AdvancedOrderIntent, reason: impl Into<String>) -> Self {
        Self {
            disposition: MatchDisposition::Expired,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: order.base.quantity,
            fill_price: None,
            visible_quantity: order.visible_quantity(),
            reason: Some(reason.into()),
        }
    }
}

fn crosses(order: &AdvancedOrderIntent, top: TopOfBook) -> bool {
    match (order.base.side, order.base.limit_price) {
        (_, None) => true,
        (Side::Buy, Some(limit)) => limit >= top.ask_price,
        (Side::Sell, Some(limit)) => limit <= top.bid_price,
    }
}

fn available(order: &AdvancedOrderIntent, top: TopOfBook) -> (Decimal, Decimal) {
    match order.base.side {
        Side::Buy => (top.ask_quantity.max(Decimal::ZERO), top.ask_price),
        Side::Sell => (top.bid_quantity.max(Decimal::ZERO), top.bid_price),
    }
}

fn reduce_only_capacity(order: &AdvancedOrderIntent, position: Decimal) -> Decimal {
    if order.base.effect != ExposureEffect::ReduceOnly {
        return order.base.quantity;
    }
    match order.base.side {
        Side::Buy if position < Decimal::ZERO => (-position).min(order.base.quantity),
        Side::Sell if position > Decimal::ZERO => position.min(order.base.quantity),
        _ => Decimal::ZERO,
    }
}

/// Deterministic L1 matching primitive used for replay/RL loops.
///
/// It intentionally models *execution semantics*, not queue position. A later L2
/// simulator can replace this without changing the Python batch protocol.
pub fn match_order(
    order: &AdvancedOrderIntent,
    top: TopOfBook,
    position: Decimal,
    now_ns: u64,
    session: SessionPhase,
) -> MatchResult {
    if let Err(error) = order.validate() {
        return MatchResult::reject(order, error.to_string());
    }

    match order.time_in_force {
        TimeInForce::Gtd { expires_at_ns } if now_ns >= expires_at_ns => {
            return MatchResult::expire(order, "GTD deadline reached");
        }
        TimeInForce::Day if session == SessionPhase::Closed => {
            return MatchResult::expire(order, "DAY order expired at session close");
        }
        TimeInForce::AtTheOpen if session != SessionPhase::OpenAuction => {
            return MatchResult {
                disposition: MatchDisposition::Resting,
                filled_quantity: Decimal::ZERO,
                remaining_quantity: order.base.quantity,
                fill_price: None,
                visible_quantity: order.visible_quantity(),
                reason: Some("waiting for open auction".into()),
            };
        }
        TimeInForce::AtTheClose if session != SessionPhase::CloseAuction => {
            return MatchResult {
                disposition: MatchDisposition::Resting,
                filled_quantity: Decimal::ZERO,
                remaining_quantity: order.base.quantity,
                fill_price: None,
                visible_quantity: order.visible_quantity(),
                reason: Some("waiting for close auction".into()),
            };
        }
        _ => {}
    }

    if order.constraints.post_only && crosses(order, top) {
        return MatchResult::reject(order, "post-only order would take liquidity");
    }

    let capacity = reduce_only_capacity(order, position);
    if order.base.effect == ExposureEffect::ReduceOnly && capacity <= Decimal::ZERO {
        return MatchResult::reject(order, "reduce-only order would not reduce exposure");
    }

    if !crosses(order, top) {
        return match order.time_in_force {
            TimeInForce::Ioc | TimeInForce::Fok => MatchResult {
                disposition: MatchDisposition::Canceled,
                filled_quantity: Decimal::ZERO,
                remaining_quantity: order.base.quantity,
                fill_price: None,
                visible_quantity: order.visible_quantity(),
                reason: Some("immediate order did not cross".into()),
            },
            _ => MatchResult {
                disposition: MatchDisposition::Resting,
                filled_quantity: Decimal::ZERO,
                remaining_quantity: order.base.quantity,
                fill_price: None,
                visible_quantity: order.visible_quantity(),
                reason: None,
            },
        };
    }

    let (book_available, price) = available(order, top);
    let order_slice = order
        .constraints
        .iceberg_display_quantity
        .unwrap_or(order.base.quantity);
    let executable = book_available
        .min(capacity)
        .min(order.base.quantity)
        .min(order_slice);

    if order.time_in_force == TimeInForce::Fok && executable < order.base.quantity {
        return MatchResult {
            disposition: MatchDisposition::Canceled,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: order.base.quantity,
            fill_price: None,
            visible_quantity: Decimal::ZERO,
            reason: Some("FOK cannot fill entire quantity".into()),
        };
    }

    if executable <= Decimal::ZERO {
        return MatchResult::reject(order, "no executable liquidity");
    }

    let remaining = order.base.quantity - executable;
    let disposition = if remaining.is_zero() {
        MatchDisposition::Filled
    } else if order.time_in_force == TimeInForce::Ioc {
        MatchDisposition::PartiallyFilledAndCanceled
    } else {
        MatchDisposition::Resting
    };

    MatchResult {
        disposition,
        filled_quantity: executable,
        remaining_quantity: remaining,
        fill_price: Some(price),
        visible_quantity: if remaining.is_zero() {
            Decimal::ZERO
        } else {
            order.visible_quantity().min(remaining)
        },
        reason: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompositeAction {
    CancelPeer {
        client_order_id: String,
    },
    ResizePeer {
        client_order_id: String,
        new_quantity: Decimal,
    },
    ActivateChild {
        client_order_id: String,
    },
}

/// Small deterministic coordinator for OCO/OUO/OTO semantics.
///
/// Registration is explicit so live venue adapters can choose native support,
/// while shadow/replay can emulate exactly the same state transitions.
#[derive(Debug, Default)]
pub struct CompositeCoordinator {
    groups: BTreeMap<String, BTreeSet<String>>,
    instruction_by_client: BTreeMap<String, CompositeInstruction>,
    quantity_by_client: BTreeMap<String, Decimal>,
    children_by_parent: BTreeMap<String, BTreeSet<String>>,
}

impl CompositeCoordinator {
    pub fn register(&mut self, order: &AdvancedOrderIntent) {
        let client_id = order.base.client_order_id();
        self.quantity_by_client
            .insert(client_id.clone(), order.base.quantity);
        self.instruction_by_client
            .insert(client_id.clone(), order.composite.clone());

        match &order.composite {
            CompositeInstruction::Oco { group_id } | CompositeInstruction::Ouo { group_id } => {
                self.groups
                    .entry(group_id.clone())
                    .or_default()
                    .insert(client_id);
            }
            CompositeInstruction::Oto {
                parent_client_order_id,
            } => {
                self.children_by_parent
                    .entry(parent_client_order_id.clone())
                    .or_default()
                    .insert(client_id);
            }
            CompositeInstruction::Single => {}
        }
    }

    pub fn on_fill(
        &self,
        client_order_id: &str,
        cumulative_filled: Decimal,
    ) -> Vec<CompositeAction> {
        let Some(instruction) = self.instruction_by_client.get(client_order_id) else {
            return Vec::new();
        };
        match instruction {
            CompositeInstruction::Oco { group_id } => self
                .groups
                .get(group_id)
                .into_iter()
                .flatten()
                .filter(|peer| peer.as_str() != client_order_id)
                .map(|peer| CompositeAction::CancelPeer {
                    client_order_id: peer.clone(),
                })
                .collect(),
            CompositeInstruction::Ouo { group_id } => self
                .groups
                .get(group_id)
                .into_iter()
                .flatten()
                .filter(|peer| peer.as_str() != client_order_id)
                .filter_map(|peer| {
                    let original = *self.quantity_by_client.get(peer)?;
                    Some(CompositeAction::ResizePeer {
                        client_order_id: peer.clone(),
                        new_quantity: (original - cumulative_filled).max(Decimal::ZERO),
                    })
                })
                .collect(),
            CompositeInstruction::Single | CompositeInstruction::Oto { .. } => {
                let parent_complete = self
                    .quantity_by_client
                    .get(client_order_id)
                    .is_some_and(|quantity| cumulative_filled >= *quantity);
                if !parent_complete {
                    return Vec::new();
                }
                self.children_by_parent
                    .get(client_order_id)
                    .into_iter()
                    .flatten()
                    .map(|child| CompositeAction::ActivateChild {
                        client_order_id: child.clone(),
                    })
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{
        Venue,
        advanced_order::{CompositeInstruction, OrderConstraints},
    };
    use uuid::Uuid;

    fn order(tif: TimeInForce, qty: i64, limit: Option<i64>) -> AdvancedOrderIntent {
        AdvancedOrderIntent {
            base: pg_types::OrderIntent {
                intent_id: Uuid::new_v4(),
                strategy_id: "sim".into(),
                asset: "BTCUSDC".into(),
                venue: Venue::BinancePm,
                side: Side::Buy,
                quantity: Decimal::from(qty),
                limit_price: limit.map(Decimal::from),
                effect: ExposureEffect::Increase,
                source_signal_id: None,
            },
            time_in_force: tif,
            constraints: OrderConstraints::default(),
            composite: CompositeInstruction::Single,
        }
    }

    fn top() -> TopOfBook {
        TopOfBook {
            bid_price: Decimal::from(99),
            bid_quantity: Decimal::from(5),
            ask_price: Decimal::from(100),
            ask_quantity: Decimal::from(4),
        }
    }

    #[test]
    fn ioc_partially_fills_and_cancels_remainder() {
        let result = match_order(
            &order(TimeInForce::Ioc, 10, Some(100)),
            top(),
            Decimal::ZERO,
            1,
            SessionPhase::Continuous,
        );
        assert_eq!(result.filled_quantity, Decimal::from(4));
        assert_eq!(result.remaining_quantity, Decimal::from(6));
        assert_eq!(
            result.disposition,
            MatchDisposition::PartiallyFilledAndCanceled
        );
    }

    #[test]
    fn fok_is_all_or_none() {
        let result = match_order(
            &order(TimeInForce::Fok, 10, Some(100)),
            top(),
            Decimal::ZERO,
            1,
            SessionPhase::Continuous,
        );
        assert_eq!(result.filled_quantity, Decimal::ZERO);
        assert_eq!(result.disposition, MatchDisposition::Canceled);
    }

    #[test]
    fn post_only_cross_is_rejected() {
        let mut value = order(TimeInForce::Gtc, 1, Some(100));
        value.constraints.post_only = true;
        let result = match_order(&value, top(), Decimal::ZERO, 1, SessionPhase::Continuous);
        assert_eq!(result.disposition, MatchDisposition::Rejected);
    }

    #[test]
    fn reduce_only_cannot_flip_position() {
        let mut value = order(TimeInForce::Ioc, 10, Some(100));
        value.base.effect = ExposureEffect::ReduceOnly;
        value.constraints.reduce_only = true;
        let result = match_order(
            &value,
            top(),
            Decimal::from(-2),
            1,
            SessionPhase::Continuous,
        );
        assert_eq!(result.filled_quantity, Decimal::from(2));
        assert_eq!(result.remaining_quantity, Decimal::from(8));
    }

    #[test]
    fn oco_fill_cancels_peer() {
        let mut left = order(TimeInForce::Gtc, 1, Some(100));
        let mut right = order(TimeInForce::Gtc, 1, Some(101));
        left.composite = CompositeInstruction::Oco {
            group_id: "g".into(),
        };
        right.composite = CompositeInstruction::Oco {
            group_id: "g".into(),
        };
        let right_id = right.base.client_order_id();
        let mut coordinator = CompositeCoordinator::default();
        coordinator.register(&left);
        coordinator.register(&right);
        assert_eq!(
            coordinator.on_fill(&left.base.client_order_id(), Decimal::ONE),
            vec![CompositeAction::CancelPeer {
                client_order_id: right_id
            }]
        );
    }
    #[test]
    fn iceberg_executes_only_current_display_slice() {
        let mut value = order(TimeInForce::Gtc, 3, Some(100));
        value.constraints.iceberg_display_quantity = Some(Decimal::ONE);
        let result = match_order(
            &value,
            TopOfBook {
                ask_quantity: Decimal::from(3),
                ..top()
            },
            Decimal::ZERO,
            1,
            SessionPhase::Continuous,
        );
        assert_eq!(result.filled_quantity, Decimal::ONE);
        assert_eq!(result.remaining_quantity, Decimal::from(2));
        assert_eq!(result.visible_quantity, Decimal::ONE);
        assert_eq!(result.disposition, MatchDisposition::Resting);
    }

    #[test]
    fn oto_child_waits_for_complete_parent_fill() {
        let parent = order(TimeInForce::Gtc, 2, Some(100));
        let parent_id = parent.base.client_order_id();
        let mut child = order(TimeInForce::Gtc, 1, Some(101));
        child.composite = CompositeInstruction::Oto {
            parent_client_order_id: parent_id.clone(),
        };
        let child_id = child.base.client_order_id();
        let mut coordinator = CompositeCoordinator::default();
        coordinator.register(&parent);
        coordinator.register(&child);

        assert!(coordinator.on_fill(&parent_id, Decimal::ONE).is_empty());
        assert_eq!(
            coordinator.on_fill(&parent_id, Decimal::from(2)),
            vec![CompositeAction::ActivateChild {
                client_order_id: child_id
            }]
        );
    }
}
