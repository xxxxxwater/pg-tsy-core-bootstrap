//! Route one venue's execution calls across per-instrument adapters.
//!
//! IBKR is the reason this exists: `IbkrExecutionAdapter` is bound to a single
//! contract, while the orchestrator addresses a `Venue`. The composite keeps the
//! venue-level contract intact and never guesses which instrument an order belongs
//! to - submit and cancel are routed by the explicit asset carried on the intent or
//! the locator, and an unknown asset is an error rather than a default.
//!
//! The instrument map uses interior synchronization so a live universe refresh can
//! add/remove contracts without replacing the venue adapter held by DurableExecution.

use crate::{
    AccountSnapshot, ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck,
    VenueOrderSnapshot, VenuePositionSnapshot,
};
use async_trait::async_trait;
use pg_types::{OrderIntent, Venue};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub struct CompositeExecutionAdapter {
    venue: Option<Venue>,
    per_asset: Arc<RwLock<BTreeMap<String, Arc<dyn ExecutionAdapter>>>>,
}

impl CompositeExecutionAdapter {
    pub fn new(venue: Venue) -> Self {
        Self {
            venue: Some(venue),
            per_asset: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub fn venue(&self) -> Option<Venue> {
        self.venue
    }

    pub fn with_instrument(
        self,
        asset: impl Into<String>,
        adapter: Arc<dyn ExecutionAdapter>,
    ) -> Self {
        self.register_instrument(asset, adapter);
        self
    }

    pub fn register_instrument(
        &self,
        asset: impl Into<String>,
        adapter: Arc<dyn ExecutionAdapter>,
    ) {
        self.per_asset
            .write()
            .expect("composite execution lock poisoned")
            .insert(asset.into(), adapter);
    }

    pub fn remove_instrument(&self, asset: &str) -> Option<Arc<dyn ExecutionAdapter>> {
        self.per_asset
            .write()
            .expect("composite execution lock poisoned")
            .remove(asset)
    }

    pub fn assets(&self) -> Vec<String> {
        self.per_asset
            .read()
            .expect("composite execution lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.per_asset
            .read()
            .expect("composite execution lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn for_asset(&self, asset: &str) -> Result<Arc<dyn ExecutionAdapter>, ExecutionError> {
        self.per_asset
            .read()
            .expect("composite execution lock poisoned")
            .get(asset)
            .cloned()
            .ok_or_else(|| {
                ExecutionError::Unsupported(format!(
                    "no execution adapter is registered for asset {asset} on venue {:?}",
                    self.venue
                ))
            })
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionAdapter>> {
        self.per_asset
            .read()
            .expect("composite execution lock poisoned")
            .values()
            .cloned()
            .collect()
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
        let mut unique = BTreeMap::<String, VenueOrderSnapshot>::new();
        for adapter in self.children() {
            for order in adapter.open_orders().await? {
                let key = order
                    .client_order_id
                    .clone()
                    .unwrap_or_else(|| format!("venue:{}", order.venue_order_id));
                if let Some(existing) = unique.get(&key) {
                    if existing.asset != order.asset
                        || existing.requested_quantity != order.requested_quantity
                        || existing.filled_quantity != order.filled_quantity
                        || existing.state != order.state
                    {
                        return Err(ExecutionError::Unknown(format!(
                            "conflicting composite order snapshots for {key}"
                        )));
                    }
                } else {
                    unique.insert(key, order);
                }
            }
        }
        Ok(unique.into_values().collect())
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        let mut unique = BTreeMap::<String, VenuePositionSnapshot>::new();
        for adapter in self.children() {
            for position in adapter.positions().await? {
                if let Some(existing) = unique.get(&position.asset) {
                    if existing.quantity != position.quantity {
                        return Err(ExecutionError::Unknown(format!(
                            "conflicting composite position snapshots for {}: {} != {}",
                            position.asset, existing.quantity, position.quantity
                        )));
                    }
                } else {
                    unique.insert(position.asset.clone(), position);
                }
            }
        }
        Ok(unique.into_values().collect())
    }

    async fn account_snapshot(&self) -> Result<AccountSnapshot, ExecutionError> {
        let adapter = self.children().into_iter().next().ok_or_else(|| {
            ExecutionError::Unsupported("composite has no instrument adapters".into())
        })?;
        adapter.account_snapshot().await
    }

    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        for adapter in self.children() {
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

        async fn account_snapshot(&self) -> Result<AccountSnapshot, ExecutionError> {
            if self.fail_reads {
                return Err(ExecutionError::Transport("read failed".into()));
            }
            Ok(AccountSnapshot {
                venue: Venue::InteractiveBrokers,
                account_id: Some("DU123".into()),
                currency: Some("USD".into()),
                account_value: Some(Decimal::from(100_000)),
                available_funds: Some(Decimal::from(50_000)),
                withdrawable: None,
                buying_power: Some(Decimal::from(200_000)),
                initial_margin: Some(Decimal::from(20_000)),
                maintenance_margin: Some(Decimal::from(15_000)),
                margin_used: None,
                gross_position_value: Some(Decimal::from(40_000)),
                raw_usd: None,
            })
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
    async fn submit_routes_by_asset_and_runtime_registration_works() {
        let apple = Arc::new(FakeAdapter::default());
        let msft = Arc::new(FakeAdapter::default());
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", apple.clone());
        composite.register_instrument("MSFT", msft.clone());

        composite
            .submit(&intent("MSFT", Venue::InteractiveBrokers))
            .await
            .unwrap();

        assert!(apple.submitted.lock().unwrap().is_empty());
        assert_eq!(msft.submitted.lock().unwrap().as_slice(), ["MSFT"]);
        assert_eq!(composite.assets(), vec!["AAPL", "MSFT"]);
        assert!(composite.remove_instrument("MSFT").is_some());
        assert_eq!(composite.assets(), vec!["AAPL"]);
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
    async fn read_side_deduplicates_shared_account_snapshots_and_propagates_errors() {
        let composite = CompositeExecutionAdapter::new(Venue::InteractiveBrokers)
            .with_instrument("AAPL", Arc::new(FakeAdapter::default()))
            .with_instrument("MSFT", Arc::new(FakeAdapter::default()));
        assert_eq!(composite.positions().await.unwrap().len(), 1);
        assert_eq!(
            composite.account_snapshot().await.unwrap().account_value,
            Some(Decimal::from(100_000))
        );

        let failing = CompositeExecutionAdapter::new(Venue::InteractiveBrokers).with_instrument(
            "AAPL",
            Arc::new(FakeAdapter {
                submitted: Mutex::new(Vec::new()),
                fail_reads: true,
            }),
        );
        assert!(failing.positions().await.is_err());
        assert!(failing.account_snapshot().await.is_err());
    }
}
