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
    PartialFill,
    Fill,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRecord {
    pub order_id: Uuid,
    pub owner_strategy_id: String,
    pub state: OrderState,
}

impl OrderRecord {
    pub fn apply(&mut self, event: OrderEvent) -> Result<(), TransitionError> {
        let next = match (self.state, event) {
            (OrderState::Created, OrderEvent::SubmitRequested) => OrderState::PendingSubmit,
            (OrderState::PendingSubmit, OrderEvent::Accepted) => OrderState::Open,
            (OrderState::PendingSubmit, OrderEvent::Rejected) => OrderState::Rejected,
            (OrderState::Open, OrderEvent::PartialFill) => OrderState::PartiallyFilled,
            (OrderState::Open | OrderState::PartiallyFilled, OrderEvent::Fill) => {
                OrderState::Filled
            }
            (
                OrderState::Open | OrderState::PartiallyFilled,
                OrderEvent::CancelRequested,
            ) => OrderState::PendingCancel,
            (OrderState::PendingCancel, OrderEvent::Canceled) => OrderState::Canceled,
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

    fn record() -> OrderRecord {
        OrderRecord {
            order_id: Uuid::new_v4(),
            owner_strategy_id: "demo".into(),
            state: OrderState::Created,
        }
    }

    #[test]
    fn accepted_order_can_fill() {
        let mut order = record();
        order.apply(OrderEvent::SubmitRequested).unwrap();
        order.apply(OrderEvent::Accepted).unwrap();
        order.apply(OrderEvent::Fill).unwrap();
        assert_eq!(order.state, OrderState::Filled);
    }

    #[test]
    fn lost_live_order_moves_unknown() {
        let mut order = record();
        order.apply(OrderEvent::SubmitRequested).unwrap();
        order.apply(OrderEvent::LostState).unwrap();
        assert_eq!(order.state, OrderState::Unknown);
    }

    #[test]
    fn terminal_order_rejects_more_transitions() {
        let mut order = record();
        order.apply(OrderEvent::SubmitRequested).unwrap();
        order.apply(OrderEvent::Rejected).unwrap();
        assert!(order.apply(OrderEvent::Accepted).is_err());
    }
}
