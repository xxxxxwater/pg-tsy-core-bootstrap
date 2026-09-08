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
        use OrderEvent::*;
        use OrderState::*;
        self.state = match (self.state, event) {
            (Created, SubmitRequested) => PendingSubmit,
            (PendingSubmit, Accepted) => Open,
            (PendingSubmit, Rejected) => Rejected,
            (Open, PartialFill) => PartiallyFilled,
            (Open | PartiallyFilled, Fill) => Filled,
            (Open | PartiallyFilled, CancelRequested) => PendingCancel,
            (PendingCancel, Canceled) => Canceled,
            (PendingSubmit | Open | PartiallyFilled | PendingCancel, LostState) => Unknown,
            _ => {
                return Err(TransitionError {
                    from: self.state,
                    event,
                });
            }
        };
        Ok(())
    }
}
