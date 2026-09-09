use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, VenueOrderSnapshot, VenueOrderState, VenuePositionSnapshot,
};
use pg_oms::{OrderEvent, OrderRecord, OrderState};
use pg_reconcile::{Ownership, ReconcileReport, VenuePosition, reconcile};
use pg_store::{PostgresStore, RuntimeLease, StoreError};
use pg_types::{OrderIntent, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeStoreError {
    #[error("runtime fencing token is no longer valid")]
    FencingLost,
    #[error("runtime store error: {0}")]
    Other(String),
}

fn map_store_error(error: StoreError) -> RuntimeStoreError {
    match error {
        StoreError::FencingLost => RuntimeStoreError::FencingLost,
        other => RuntimeStoreError::Other(other.to_string()),
    }
}

#[async_trait]
pub trait RuntimeStore: Send + Sync {
    async fn assert_lease(&self, lease: &RuntimeLease) -> Result<(), RuntimeStoreError>;
    async fn append_event(
        &self,
        stream_id: &str,
        event_type: &str,
        payload: &Value,
        fencing_token: i64,
    ) -> Result<i64, RuntimeStoreError>;
    async fn save_order_record(
        &self,
        record: &OrderRecord,
        fencing_token: i64,
    ) -> Result<(), RuntimeStoreError>;
    async fn load_orders_for_venue(
        &self,
        venue: Venue,
    ) -> Result<Vec<OrderRecord>, RuntimeStoreError>;
    async fn load_position_states(
        &self,
        venue: Venue,
    ) -> Result<Vec<VenuePosition>, RuntimeStoreError>;
    async fn save_position_state(
        &self,
        position: &VenuePosition,
        fencing_token: i64,
    ) -> Result<(), RuntimeStoreError>;
    async fn save_reconcile_report(
        &self,
        venue: Venue,
        report: &ReconcileReport,
        fencing_token: i64,
    ) -> Result<Uuid, RuntimeStoreError>;
}

#[async_trait]
impl RuntimeStore for PostgresStore {
    async fn assert_lease(&self, lease: &RuntimeLease) -> Result<(), RuntimeStoreError> {
        PostgresStore::assert_lease(self, lease)
            .await
            .map_err(map_store_error)
    }

    async fn append_event(
        &self,
        stream_id: &str,
        event_type: &str,
        payload: &Value,
        fencing_token: i64,
    ) -> Result<i64, RuntimeStoreError> {
        PostgresStore::append_event(self, stream_id, event_type, payload, fencing_token)
            .await
            .map_err(map_store_error)
    }

    async fn save_order_record(
        &self,
        record: &OrderRecord,
        fencing_token: i64,
    ) -> Result<(), RuntimeStoreError> {
        PostgresStore::save_order_record(self, record, fencing_token)
            .await
            .map_err(map_store_error)
    }

    async fn load_orders_for_venue(
        &self,
        venue: Venue,
    ) -> Result<Vec<OrderRecord>, RuntimeStoreError> {
        PostgresStore::load_orders_for_venue(self, venue)
            .await
            .map_err(map_store_error)
    }

    async fn load_position_states(
        &self,
        venue: Venue,
    ) -> Result<Vec<VenuePosition>, RuntimeStoreError> {
        PostgresStore::load_position_states(self, venue)
            .await
            .map_err(map_store_error)
    }

    async fn save_position_state(
        &self,
        position: &VenuePosition,
        fencing_token: i64,
    ) -> Result<(), RuntimeStoreError> {
        PostgresStore::save_position_state(self, position, fencing_token)
            .await
            .map_err(map_store_error)
    }

    async fn save_reconcile_report(
        &self,
        venue: Venue,
        report: &ReconcileReport,
        fencing_token: i64,
    ) -> Result<Uuid, RuntimeStoreError> {
        PostgresStore::save_reconcile_report(self, venue, report, fencing_token)
            .await
            .map_err(map_store_error)
    }
}

#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<Venue, Arc<dyn ExecutionAdapter>>,
}

impl AdapterRegistry {
    pub fn register(&mut self, venue: Venue, adapter: Arc<dyn ExecutionAdapter>) {
        self.adapters.insert(venue, adapter);
    }

    pub fn get(&self, venue: Venue) -> Option<Arc<dyn ExecutionAdapter>> {
        self.adapters.get(&venue).cloned()
    }

    pub fn venues(&self) -> Vec<Venue> {
        self.adapters.keys().copied().collect()
    }
}

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Store(#[from] RuntimeStoreError),
    #[error("execution adapter is not registered for {0:?}")]
    MissingAdapter(Venue),
    #[error("OMS transition failed: {0}")]
    Oms(String),
    #[error("order outcome is ambiguous for {client_order_id}: {reason}")]
    Ambiguous {
        client_order_id: String,
        reason: String,
    },
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryItem {
    pub client_order_id: String,
    pub state: OrderState,
    pub venue_order_id: Option<String>,
    pub filled_quantity: Decimal,
    pub resolved: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub venue: Option<Venue>,
    pub items: Vec<RecoveryItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileCycle {
    pub venue: Venue,
    pub report: ReconcileReport,
    pub orders: Vec<OrderRecord>,
    pub positions: Vec<VenuePosition>,
}

pub struct DurableExecution<S: RuntimeStore> {
    store: Arc<S>,
    lease: RuntimeLease,
    adapters: AdapterRegistry,
}

impl<S: RuntimeStore> DurableExecution<S> {
    pub fn new(store: Arc<S>, lease: RuntimeLease, adapters: AdapterRegistry) -> Self {
        Self {
            store,
            lease,
            adapters,
        }
    }

    pub fn lease(&self) -> &RuntimeLease {
        &self.lease
    }

    pub fn adapters(&self) -> &AdapterRegistry {
        &self.adapters
    }

    pub async fn dispatch(&self, intent: &OrderIntent) -> Result<OrderRecord, OrchestratorError> {
        self.store.assert_lease(&self.lease).await?;
        let adapter = self
            .adapters
            .get(intent.venue)
            .ok_or(OrchestratorError::MissingAdapter(intent.venue))?;
        let stream_id = format!("order:{}", intent.client_order_id());
        let mut record = OrderRecord::from_intent(intent);

        // The local record exists before any network side effect. A crash after this point
        // can therefore recover using the stable client id without inventing a new intent.
        self.store
            .save_order_record(&record, self.lease.fencing_token)
            .await?;
        self.store
            .append_event(
                &stream_id,
                "order.intent.persisted",
                &json!({"intent": intent, "client_order_id": record.client_order_id}),
                self.lease.fencing_token,
            )
            .await?;

        record
            .apply(OrderEvent::SubmitRequested)
            .map_err(|error| OrchestratorError::Oms(error.to_string()))?;
        self.store
            .save_order_record(&record, self.lease.fencing_token)
            .await?;
        self.store
            .append_event(
                &stream_id,
                "order.dispatch.started",
                &json!({"client_order_id": record.client_order_id}),
                self.lease.fencing_token,
            )
            .await?;

        // Fencing is checked again immediately before the external side effect.
        self.store.assert_lease(&self.lease).await?;

        match adapter.submit(intent).await {
            Ok(ack) => {
                record
                    .accept(ack.venue_order_id.clone())
                    .map_err(|error| OrchestratorError::Oms(error.to_string()))?;
                self.store
                    .save_order_record(&record, self.lease.fencing_token)
                    .await?;
                self.store
                    .append_event(
                        &stream_id,
                        "order.dispatch.acknowledged",
                        &json!({
                            "client_order_id": ack.client_order_id,
                            "venue_order_id": ack.venue_order_id,
                        }),
                        self.lease.fencing_token,
                    )
                    .await?;
                Ok(record)
            }
            Err(error @ (ExecutionError::Unknown(_) | ExecutionError::Transport(_))) => {
                record
                    .apply(OrderEvent::LostState)
                    .map_err(|transition| OrchestratorError::Oms(transition.to_string()))?;
                self.store
                    .save_order_record(&record, self.lease.fencing_token)
                    .await?;
                self.store
                    .append_event(
                        &stream_id,
                        "order.dispatch.unknown",
                        &json!({
                            "client_order_id": record.client_order_id,
                            "reason": error.to_string(),
                        }),
                        self.lease.fencing_token,
                    )
                    .await?;
                Err(OrchestratorError::Ambiguous {
                    client_order_id: record.client_order_id.clone(),
                    reason: error.to_string(),
                })
            }
            Err(error) => {
                record
                    .apply(OrderEvent::Rejected)
                    .map_err(|transition| OrchestratorError::Oms(transition.to_string()))?;
                self.store
                    .save_order_record(&record, self.lease.fencing_token)
                    .await?;
                self.store
                    .append_event(
                        &stream_id,
                        "order.dispatch.rejected",
                        &json!({
                            "client_order_id": record.client_order_id,
                            "reason": error.to_string(),
                        }),
                        self.lease.fencing_token,
                    )
                    .await?;
                Err(OrchestratorError::Execution(error))
            }
        }
    }

    /// Resolve PendingSubmit/Unknown records by stable client id. This method never
    /// calls submit, so recovery cannot create duplicate exposure.
    pub async fn recover_ambiguous(
        &self,
        venue: Venue,
    ) -> Result<RecoveryReport, OrchestratorError> {
        self.store.assert_lease(&self.lease).await?;
        let adapter = self
            .adapters
            .get(venue)
            .ok_or(OrchestratorError::MissingAdapter(venue))?;
        let mut orders = self.store.load_orders_for_venue(venue).await?;
        let mut report = RecoveryReport {
            venue: Some(venue),
            items: Vec::new(),
        };

        for order in orders.iter_mut().filter(|order| {
            matches!(order.state, OrderState::PendingSubmit | OrderState::Unknown)
        }) {
            let stream_id = format!("order:{}", order.client_order_id);
            let remote = adapter
                .find_order_by_client_id(&order.client_order_id)
                .await?;
            let resolved = match remote {
                Some(snapshot) => {
                    apply_snapshot(order, &snapshot);
                    self.store
                        .append_event(
                            &stream_id,
                            "order.recovery.resolved",
                            &json!({"snapshot": snapshot}),
                            self.lease.fencing_token,
                        )
                        .await?;
                    true
                }
                None => {
                    order.state = OrderState::Unknown;
                    self.store
                        .append_event(
                            &stream_id,
                            "order.recovery.unresolved",
                            &json!({
                                "client_order_id": order.client_order_id,
                                "action": "safe_hold_no_resubmit",
                            }),
                            self.lease.fencing_token,
                        )
                        .await?;
                    false
                }
            };
            self.store
                .save_order_record(order, self.lease.fencing_token)
                .await?;
            report.items.push(RecoveryItem {
                client_order_id: order.client_order_id.clone(),
                state: order.state,
                venue_order_id: order.venue_order_id.clone(),
                filled_quantity: order.filled_quantity,
                resolved,
            });
        }
        Ok(report)
    }

    pub async fn reconcile_once(
        &self,
        venue: Venue,
    ) -> Result<ReconcileCycle, OrchestratorError> {
        self.store.assert_lease(&self.lease).await?;
        let adapter = self
            .adapters
            .get(venue)
            .ok_or(OrchestratorError::MissingAdapter(venue))?;

        let mut local_orders = self.store.load_orders_for_venue(venue).await?;
        let mut venue_orders = adapter.open_orders().await?;

        // An order can fill between snapshots. Resolve every non-terminal local order
        // that disappeared from open_orders against venue history before declaring drift.
        for local in local_orders.iter().filter(|order| !order.is_terminal()) {
            let already_present = venue_orders.iter().any(|remote| {
                remote.client_order_id.as_deref() == Some(local.client_order_id.as_str())
            });
            if !already_present
                && let Some(remote) = adapter
                    .find_order_by_client_id(&local.client_order_id)
                    .await?
            {
                venue_orders.push(remote);
            }
        }

        for local in &mut local_orders {
            if let Some(remote) = venue_orders.iter().find(|remote| {
                remote.client_order_id.as_deref() == Some(local.client_order_id.as_str())
            }) {
                apply_snapshot(local, remote);
                self.store
                    .save_order_record(local, self.lease.fencing_token)
                    .await?;
            }
        }

        let stored_positions = self.store.load_position_states(venue).await?;
        let ownership = stored_positions
            .into_iter()
            .map(|position| (position.asset, position.ownership))
            .collect::<BTreeMap<_, _>>();
        let venue_positions = adapter.positions().await?;
        let positions = merge_position_ownership(venue, venue_positions, ownership);
        for position in &positions {
            self.store
                .save_position_state(position, self.lease.fencing_token)
                .await?;
        }

        let report = reconcile(venue, &local_orders, &venue_orders, &positions);
        self.store
            .save_reconcile_report(venue, &report, self.lease.fencing_token)
            .await?;
        self.store
            .append_event(
                &format!("reconcile:{venue:?}"),
                "reconcile.completed",
                &serde_json::to_value(&report).map_err(|error| {
                    RuntimeStoreError::Other(format!("reconcile serialization failed: {error}"))
                })?,
                self.lease.fencing_token,
            )
            .await?;

        Ok(ReconcileCycle {
            venue,
            report,
            orders: local_orders,
            positions,
        })
    }
}

fn apply_snapshot(record: &mut OrderRecord, snapshot: &VenueOrderSnapshot) {
    if snapshot.requested_quantity != record.requested_quantity
        || snapshot.filled_quantity < Decimal::ZERO
        || snapshot.filled_quantity > record.requested_quantity
    {
        record.state = OrderState::Unknown;
        return;
    }
    record.venue_order_id = Some(snapshot.venue_order_id.clone());
    record.filled_quantity = snapshot.filled_quantity;
    record.state = match snapshot.state {
        VenueOrderState::Open => OrderState::Open,
        VenueOrderState::PartiallyFilled => OrderState::PartiallyFilled,
        VenueOrderState::Filled => OrderState::Filled,
        VenueOrderState::Canceled => OrderState::Canceled,
        VenueOrderState::Rejected => OrderState::Rejected,
        VenueOrderState::Unknown => OrderState::Unknown,
    };
}

fn merge_position_ownership(
    venue: Venue,
    snapshots: Vec<VenuePositionSnapshot>,
    mut ownership: BTreeMap<String, Ownership>,
) -> Vec<VenuePosition> {
    let mut positions = Vec::with_capacity(snapshots.len() + ownership.len());
    for snapshot in snapshots {
        let owner = ownership
            .remove(&snapshot.asset)
            .unwrap_or(Ownership::Unknown);
        positions.push(VenuePosition {
            venue,
            asset: snapshot.asset,
            quantity: snapshot.quantity,
            ownership: owner,
        });
    }
    for (asset, owner) in ownership {
        positions.push(VenuePosition {
            venue,
            asset,
            quantity: Decimal::ZERO,
            ownership: owner,
        });
    }
    positions
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_execution::{OrderLocator, VenueOrderAck};
    use pg_types::{ExposureEffect, Side};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Clone, Copy)]
    enum SubmitMode {
        Ack,
        PublishThenUnknown,
        UnknownWithoutPublish,
    }

    struct FakeAdapter {
        mode: SubmitMode,
        submit_count: AtomicUsize,
        orders: Mutex<BTreeMap<String, VenueOrderSnapshot>>,
    }

    impl FakeAdapter {
        fn new(mode: SubmitMode) -> Self {
            Self {
                mode,
                submit_count: AtomicUsize::new(0),
                orders: Mutex::new(BTreeMap::new()),
            }
        }
    }

    #[async_trait]
    impl ExecutionAdapter for FakeAdapter {
        async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
            self.submit_count.fetch_add(1, Ordering::SeqCst);
            let client = intent.client_order_id();
            let snapshot = VenueOrderSnapshot {
                venue_order_id: "venue-1".into(),
                client_order_id: Some(client.clone()),
                asset: intent.asset.clone(),
                side: intent.side,
                requested_quantity: intent.quantity,
                filled_quantity: Decimal::ZERO,
                limit_price: intent.limit_price,
                state: VenueOrderState::Open,
            };
            match self.mode {
                SubmitMode::Ack => {
                    self.orders
                        .lock()
                        .unwrap()
                        .insert(client.clone(), snapshot);
                    Ok(VenueOrderAck {
                        venue_order_id: "venue-1".into(),
                        client_order_id: client,
                    })
                }
                SubmitMode::PublishThenUnknown => {
                    self.orders
                        .lock()
                        .unwrap()
                        .insert(client, snapshot);
                    Err(ExecutionError::Unknown("ack lost".into()))
                }
                SubmitMode::UnknownWithoutPublish => {
                    Err(ExecutionError::Unknown("network cut".into()))
                }
            }
        }

        async fn cancel(&self, _order: OrderLocator<'_>) -> Result<(), ExecutionError> {
            Ok(())
        }

        async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
            Ok(self.orders.lock().unwrap().values().cloned().collect())
        }

        async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
            Ok(Vec::new())
        }

        async fn find_order_by_client_id(
            &self,
            client_order_id: &str,
        ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
            Ok(self.orders.lock().unwrap().get(client_order_id).cloned())
        }
    }

    struct MemoryStore {
        fenced: AtomicBool,
        orders: Mutex<BTreeMap<String, OrderRecord>>,
        events: Mutex<Vec<String>>,
        positions: Mutex<BTreeMap<String, VenuePosition>>,
    }

    impl MemoryStore {
        fn new() -> Self {
            Self {
                fenced: AtomicBool::new(false),
                orders: Mutex::new(BTreeMap::new()),
                events: Mutex::new(Vec::new()),
                positions: Mutex::new(BTreeMap::new()),
            }
        }
    }

    #[async_trait]
    impl RuntimeStore for MemoryStore {
        async fn assert_lease(&self, _lease: &RuntimeLease) -> Result<(), RuntimeStoreError> {
            if self.fenced.load(Ordering::SeqCst) {
                Err(RuntimeStoreError::FencingLost)
            } else {
                Ok(())
            }
        }

        async fn append_event(
            &self,
            _stream_id: &str,
            event_type: &str,
            _payload: &Value,
            _fencing_token: i64,
        ) -> Result<i64, RuntimeStoreError> {
            let mut events = self.events.lock().unwrap();
            events.push(event_type.into());
            Ok(events.len() as i64)
        }

        async fn save_order_record(
            &self,
            record: &OrderRecord,
            _fencing_token: i64,
        ) -> Result<(), RuntimeStoreError> {
            self.orders
                .lock()
                .unwrap()
                .insert(record.client_order_id.clone(), record.clone());
            Ok(())
        }

        async fn load_orders_for_venue(
            &self,
            venue: Venue,
        ) -> Result<Vec<OrderRecord>, RuntimeStoreError> {
            Ok(self
                .orders
                .lock()
                .unwrap()
                .values()
                .filter(|order| order.venue == venue)
                .cloned()
                .collect())
        }

        async fn load_position_states(
            &self,
            venue: Venue,
        ) -> Result<Vec<VenuePosition>, RuntimeStoreError> {
            Ok(self
                .positions
                .lock()
                .unwrap()
                .values()
                .filter(|position| position.venue == venue)
                .cloned()
                .collect())
        }

        async fn save_position_state(
            &self,
            position: &VenuePosition,
            _fencing_token: i64,
        ) -> Result<(), RuntimeStoreError> {
            self.positions
                .lock()
                .unwrap()
                .insert(position.asset.clone(), position.clone());
            Ok(())
        }

        async fn save_reconcile_report(
            &self,
            _venue: Venue,
            _report: &ReconcileReport,
            _fencing_token: i64,
        ) -> Result<Uuid, RuntimeStoreError> {
            Ok(Uuid::new_v4())
        }
    }

    fn lease() -> RuntimeLease {
        RuntimeLease {
            lease_key: "test".into(),
            holder_id: "instance-a".into(),
            fencing_token: 7,
            ttl_seconds: 15,
        }
    }

    fn intent() -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: "s1".into(),
            asset: "HYPE".into(),
            venue: Venue::Hyperliquid,
            side: Side::Buy,
            quantity: Decimal::ONE,
            limit_price: Some(Decimal::from(10)),
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        }
    }

    #[tokio::test]
    async fn ambiguous_submit_recovers_without_second_submit() {
        let store = Arc::new(MemoryStore::new());
        let adapter = Arc::new(FakeAdapter::new(SubmitMode::PublishThenUnknown));
        let mut adapters = AdapterRegistry::default();
        adapters.register(Venue::Hyperliquid, adapter.clone());
        let execution = DurableExecution::new(store, lease(), adapters);
        let order_intent = intent();

        assert!(matches!(
            execution.dispatch(&order_intent).await,
            Err(OrchestratorError::Ambiguous { .. })
        ));
        assert_eq!(adapter.submit_count.load(Ordering::SeqCst), 1);

        let recovery = execution
            .recover_ambiguous(Venue::Hyperliquid)
            .await
            .unwrap();
        assert_eq!(recovery.items.len(), 1);
        assert!(recovery.items[0].resolved);
        assert_eq!(recovery.items[0].state, OrderState::Open);
        assert_eq!(adapter.submit_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unresolved_ambiguous_never_resubmits() {
        let store = Arc::new(MemoryStore::new());
        let adapter = Arc::new(FakeAdapter::new(SubmitMode::UnknownWithoutPublish));
        let mut adapters = AdapterRegistry::default();
        adapters.register(Venue::Hyperliquid, adapter.clone());
        let execution = DurableExecution::new(store, lease(), adapters);

        assert!(execution.dispatch(&intent()).await.is_err());
        let recovery = execution
            .recover_ambiguous(Venue::Hyperliquid)
            .await
            .unwrap();
        assert_eq!(recovery.items[0].state, OrderState::Unknown);
        assert!(!recovery.items[0].resolved);
        assert_eq!(adapter.submit_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fencing_loss_blocks_network_dispatch() {
        let store = Arc::new(MemoryStore::new());
        store.fenced.store(true, Ordering::SeqCst);
        let adapter = Arc::new(FakeAdapter::new(SubmitMode::Ack));
        let mut adapters = AdapterRegistry::default();
        adapters.register(Venue::Hyperliquid, adapter.clone());
        let execution = DurableExecution::new(store, lease(), adapters);

        assert!(matches!(
            execution.dispatch(&intent()).await,
            Err(OrchestratorError::Store(RuntimeStoreError::FencingLost))
        ));
        assert_eq!(adapter.submit_count.load(Ordering::SeqCst), 0);
    }
}
