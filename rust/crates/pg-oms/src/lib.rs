use pg_types::OrderIntent;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    Created,
    PendingSubmit,
    Open,
    PartiallyFilled,
    PendingCancel,
    Filled,
    Canceled,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderEvent {
    SubmitRequested,
    Accepted,
    CancelRequested,
    Canceled,
    Rejected,
    LostState,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal order transition from {from:?} on {event:?}")]
pub struct TransitionError {
    pub from: OrderState,
    pub event: OrderEvent,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FillError {
    #[error("fill quantity must be positive")]
    NonPositive,
    #[error("fill would overfill order: requested={requested}, already_filled={already_filled}, incoming={incoming}")]
    Overfill {
        requested: Decimal,
        already_filled: Decimal,
        incoming: Decimal,
    },
    #[error("fill is illegal while order is in {0:?}")]
    IllegalState(OrderState),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRecord {
    pub order_id: Uuid,
    pub intent_id: Uuid,
    pub client_order_id: String,
    pub venue_order_id: Option<String>,
    pub owner_strategy_id: String,
    pub requested_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub state: OrderState,
}

impl OrderRecord {
    pub fn from_intent(intent: &OrderIntent) -> Self {
        Self {
            order_id: Uuid::new_v4(),
            intent_id: intent.intent_id,
            client_order_id: intent.client_order_id(),
            venue_order_id: None,
            owner_strategy_id: intent.strategy_id.clone(),
            requested_quantity: intent.quantity,
            filled_quantity: Decimal::ZERO,
            state: OrderState::Created,
        }
    }

    pub fn remaining_quantity(&self) -> Decimal {
        self.requested_quantity - self.filled_quantity
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OrderState::Filled | OrderState::Canceled | OrderState::Rejected
        )
    }

    pub fn accept(&mut self, venue_order_id: impl Into<String>) -> Result<(), TransitionError> {
        self.apply(OrderEvent::Accepted)?;
        self.venue_order_id = Some(venue_order_id.into());
        Ok(())
    }

    pub fn apply_fill(&mut self, quantity: Decimal) -> Result<(), FillError> {
        if quantity <= Decimal::ZERO {
            return Err(FillError::NonPositive);
        }
        if !matches!(
            self.state,
            OrderState::Open | OrderState::PartiallyFilled | OrderState::PendingCancel
        ) {
            return Err(FillError::IllegalState(self.state));
        }

        let next_filled = self.filled_quantity + quantity;
        if next_filled > self.requested_quantity {
            return Err(FillError::Overfill {
                requested: self.requested_quantity,
                already_filled: self.filled_quantity,
                incoming: quantity,
            });
        }

        self.filled_quantity = next_filled;
        self.state = if next_filled == self.requested_quantity {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        Ok(())
    }

    pub fn apply(&mut self, event: OrderEvent) -> Result<(), TransitionError> {
        let next = match (self.state, event) {
            (OrderState::Created, OrderEvent::SubmitRequested) => OrderState::PendingSubmit,
            (OrderState::PendingSubmit, OrderEvent::Accepted) => OrderState::Open,
            (OrderState::PendingSubmit, OrderEvent::Rejected) => OrderState::Rejected,
            (
                OrderState::Open | OrderState::PartiallyFilled,
                OrderEvent::CancelRequested,
            ) => OrderState::PendingCancel,
            (
                OrderState::Open | OrderState::PartiallyFilled | OrderState::PendingCancel,
                OrderEvent::Canceled,
            ) => OrderState::Canceled,
            (
                OrderState::PendingSubmit
                | OrderState::Open
                | OrderState::PartiallyFilled
                | OrderState::PendingCancel,
                OrderEvent::LostState,
            ) => OrderState::Unknown,
            _ => {
                return Err(TransitionError {
                    from: self.state,
                    event,
                });
            }
        };
        self.state = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{ExposureEffect, Side, Venue};

    fn intent(quantity: i64) -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: "demo".into(),
            asset: "HYPE".into(),
            venue: Venue::Hyperliquid,
            side: Side::Buy,
            quantity: Decimal::from(quantity),
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        }
    }

    fn open_record(quantity: i64) -> OrderRecord {
        let mut order = OrderRecord::from_intent(&intent(quantity));
        order.apply(OrderEvent::SubmitRequested).unwrap();
        order.accept("venue-1").unwrap();
        order
    }

    #[test]
    fn partial_fill_tracks_remaining_quantity() {
        let mut order = open_record(10);
        order.apply_fill(Decimal::from(3)).unwrap();
        assert_eq!(order.state, OrderState::PartiallyFilled);
        assert_eq!(order.remaining_quantity(), Decimal::from(7));
        order.apply_fill(Decimal::from(7)).unwrap();
        assert_eq!(order.state, OrderState::Filled);
        assert!(order.is_terminal());
    }

    #[test]
    fn fill_can_arrive_after_cancel_request() {
        let mut order = open_record(10);
        order.apply(OrderEvent::CancelRequested).unwrap();
        order.apply_fill(Decimal::from(10)).unwrap();
        assert_eq!(order.state, OrderState::Filled);
    }

    #[test]
    fn overfill_is_rejected_without_mutating_quantity() {
        let mut order = open_record(10);
        order.apply_fill(Decimal::from(8)).unwrap();
        assert!(matches!(
            order.apply_fill(Decimal::from(3)),
            Err(FillError::Overfill { .. })
        ));
        assert_eq!(order.filled_quantity, Decimal::from(8));
    }

    #[test]
    fn lost_live_order_moves_unknown() {
        let mut order = OrderRecord::from_intent(&intent(1));
        order.apply(OrderEvent::SubmitRequested).unwrap();
        order.apply(OrderEvent::LostState).unwrap();
        assert_eq!(order.state, OrderState::Unknown);
    }
}
