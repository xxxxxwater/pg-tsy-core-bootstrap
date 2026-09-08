use async_trait::async_trait;
use pg_types::OrderIntent;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct VenueOrderAck { pub venue_order_id: String }

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("venue rejected order: {0}")] Rejected(String),
    #[error("venue outcome is unknown: {0}")] Unknown(String),
    #[error("transport error: {0}")] Transport(String),
}

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError>;
    async fn cancel(&self, venue_order_id: &str) -> Result<(), ExecutionError>;
}
