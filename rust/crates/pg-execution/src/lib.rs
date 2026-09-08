use async_trait::async_trait;
use pg_types::{OrderIntent, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueOrderAck {
    pub venue_order_id: String,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VenueOrderState {
    Open,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueOrderSnapshot {
    pub venue_order_id: String,
    pub client_order_id: Option<String>,
    pub asset: String,
    pub side: Side,
    pub requested_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub limit_price: Option<Decimal>,
    pub state: VenueOrderState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenuePositionSnapshot {
    pub asset: String,
    pub quantity: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderLocator<'a> {
    pub venue_order_id: Option<&'a str>,
    pub client_order_id: &'a str,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("venue rejected order: {0}")]
    Rejected(String),
    #[error("venue outcome is unknown: {0}")]
    Unknown(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("venue authentication failed: {0}")]
    Authentication(String),
    #[error("venue protocol conversion failed: {0}")]
    Conversion(String),
    #[error("operation is not supported by this venue adapter: {0}")]
    Unsupported(String),
}

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    /// Submit must use `intent.client_order_id()` at the venue whenever the venue
    /// provides a client/reference id field. A transport timeout is `Unknown`,
    /// not proof that the venue rejected the order.
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError>;

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError>;

    /// Read-side methods are required for cold-start and continuous reconciliation.
    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError>;

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError>;

    /// Resolve an ambiguous submit/cancel by the stable client id before retrying.
    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        Ok(self
            .open_orders()
            .await?
            .into_iter()
            .find(|order| order.client_order_id.as_deref() == Some(client_order_id)))
    }
}
