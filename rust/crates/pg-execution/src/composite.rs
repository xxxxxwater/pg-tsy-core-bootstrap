//! Route one venue's execution calls across per-instrument adapters.
//!
//! IBKR is the reason this exists: `IbkrExecutionAdapter` is bound to a single
//! contract, while the orchestrator addresses a `Venue`. The composite keeps the
//! venue-level contract intact and never guesses which instrument an order belongs
//! to - submit and cancel are routed by the explicit asset carried on the intent or
//! the locator, and an unknown asset is an error rather than a default.
//!
//! Read-side calls aggregate across children. A child error is propagated, never
//! swallowed: a partial venue snapshot must not be mistaken for a complete one.

use crate::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenuePositionSnapshot,
};
use async_trait::async_trait;
use pg_types::{OrderIntent, Venue};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Default)]
pub struct CompositeExecutionAdapter {
    venue: Option<Venue>,
    per_asset: BTreeMap<String, Arc<dyn ExecutionAdapter>>,
}

impl CompositeExecutionAdapter {
    pub fn new(venue: Venue) -> Self {
        Self {
            venue: Some(venue),
            per_asset: BTreeMap::new(),
        }
    }

    pub fn venue(&self) -> Option<Venue> {
        self.venue
    }

    /// Register the adapter that owns one instrument.
    pub fn with_instrument(
        mut self,
        asset: impl Into<String>,
        adapter: Arc<dyn ExecutionAdapter>,
    ) -> Self {
        self.per_asset.insert(asset.into(), adapter);
        self
    }

    pub fn assets(&self) -> Vec<String> {
        self.per_asset.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.per_asset.len()
    }

    pub fn is_empty(&self) -> bool {
        self.per_asset.is_empty()
    }

    fn for_asset(&self, asset: &str) -> Result<&Arc<dyn ExecutionAdapter>, ExecutionError> {
        self.per_asset.get(asset).ok_or_else(|| {
            ExecutionError::Unsupported(format!(
                "no execution adapter is registered for asset {asset} on venue {:?}",
                self.venue
            ))
        })
    }
}

#[async_trait]
impl ExecutionAdapter for CompositeExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        if let Some(venue) = self.venue
            && intent.venue != venue
        {
            return Err(ExecutionError::Unsupported(format!(
                "composite adapter for {venue:?} received an intent for {:?}",
                intent.venue
            )));
        }
        self.for_asset(&intent.asset)?.submit(intent).await
    }

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        self.for_asset(order.asset)?.cancel(order).await
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        let mut orders = Vec::new();
        for adapter in self.per_asset.values() {
            orders.extend(adapter.open_orders().await?);
        }
        Ok(orders)
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        let mut positions = Vec::new();
        for adapter in self.per_asset.values() {
            positions.extend(adapter.positions().await?);
        }
        Ok(positions)
    }

    /// The client id alone does not say which instrument an order belongs to, so
    /// every child is searched. This is the recovery path for a fill that already
    /// left the open-order set, so returning None must mean genuinely not found.
    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        for adapter in self.per_asset.values() {
            if let Some(order) = adapter.find_order_by_client_id(client_order_id).await? {
                return Ok(Some(order));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::sync::Mutex;

    /// Minimal recording adapter: proves routing, not venue semantics.
    #[derive(Default)]
    struct FakeAdapter {
        submitted: Mutex<Vec<String>>,
        fail_reads: bool,
    }

    #[async_trait]
    impl ExecutionAdapter for FakeAdapter {
        async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
            self.submitted.lock().unwrap().push(intent.asset.clone());
            Ok(VenueOrderAck {
                venue_order_id: format!("fake-{}", intent.asset),
                client_order_id: intent.client_order_id(),
            })
        }

        async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
            if order.asset.is_empty() {
                return Err(ExecutionError::Rejected("missing asset".into()));
            }
            Ok(())
        }

        async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
            if self.fail_reads {
                return Err(ExecutionError::Transport("read failed".into()));
            }
            Ok(Vec::new())
        }

        async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
            if self.fail_reads {
                return Err(ExecutionError::Transport("read failed".into()));
            }
            Ok(vec![VenuePositionSnapshot {
                asset: "AAPL".into(),
                quantity: Decimal::ONE,
            }])
        }
    }

    fn intent(asset: &str, venue: Venue) -> OrderIntent {
        OrderIntent {
            intent_id: uuid::Uuid::new_v4(),
            strategy_id: "composite-test".into(),
            asset: asset.into(),
            venue,
            side: pg_types::Side::Buy,
            quantity: Decimal::ONE,
            limit_price: None,
            effect: pg_types::ExposureEffect::Increase,
            source_signal_id: None,
        }
    }

    #[tokio::test]
    async fn submit_routes_by_asset() {
        let apple = Arc::new(FakeAdapter::default());
        let msft = Arc::new(FakeAdapter::default());
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", apple.clone())
            .with_instrument("MSFT", msft.clone());

        composite
            .submit(&intent("MSFT", Venue::InteractiveBrokers))
            .await
            .unwrap();

        assert!(apple.submitted.lock().unwrap().is_empty());
        assert_eq!(msft.submitted.lock().unwrap().as_slice(), ["MSFT"]);
        assert_eq!(composite.assets(), vec!["AAPL", "MSFT"]);
    }

    #[tokio::test]
    async fn unknown_asset_is_refused_rather_than_defaulted() {
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", Arc::new(FakeAdapter::default()));
        let error = composite
            .submit(&intent("TSLA", Venue::InteractiveBrokers))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Unsupported(_)), "{error:?}");
    }

    #[tokio::test]
    async fn wrong_venue_is_refused() {
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", Arc::new(FakeAdapter::default()));
        let error = composite
            .submit(&intent("AAPL", Venue::Hyperliquid))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Unsupported(_)), "{error:?}");
    }

    #[tokio::test]
    async fn read_side_aggregates_and_propagates_errors() {
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", Arc::new(FakeAdapter::default()));
        assert_eq!(composite.positions().await.unwrap().len(), 1);

        let failing = CompositeExecutionAdapter::new(Venue::InteractiveBrokers).with_instrument(
            "AAPL",
            Arc::new(FakeAdapter {
                submitted: Mutex::new(Vec::new()),
                fail_reads: true,
            }),
        );
        // A partial snapshot must never be reported as a complete one.
        assert!(failing.positions().await.is_err());
    }
}
