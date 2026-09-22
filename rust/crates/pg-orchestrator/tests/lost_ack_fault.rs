//! Isolated fault injection against the real DurableExecution algorithm.
//! No live keys or network. This proves recovery itself does not re-submit;
//! it does not prove every real exchange/network crash window.
use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenueOrderState, VenuePositionSnapshot,
};
use pg_oms::{OrderRecord, OrderState};
use pg_orchestrator::{AdapterRegistry, DurableExecution, RuntimeStore, RuntimeStoreError};
use pg_reconcile::{ReconcileReport, VenuePosition};
use pg_store::RuntimeLease;
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use uuid::Uuid;

#[derive(Default)]
struct LostAckVenue {
    accepted: Mutex<Option<VenueOrderSnapshot>>,
    hide_history: AtomicBool,
    posts: AtomicUsize,
}

#[async_trait]
impl ExecutionAdapter for LostAckVenue {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        *self.accepted.lock().unwrap() = Some(VenueOrderSnapshot {
            venue_order_id: "42".into(),
            client_order_id: Some(intent.client_order_id()),
            asset: intent.asset.clone(),
            side: intent.side,
            requested_quantity: intent.quantity,
            filled_quantity: Decimal::ZERO,
            limit_price: intent.limit_price,
            state: VenueOrderState::Open,
        });
        Err(ExecutionError::Unknown(
            "accepted by exchange, ACK dropped".into(),
        ))
    }
    async fn cancel(&self, _: OrderLocator<'_>) -> Result<(), ExecutionError> {
        panic!("recovery must never cancel")
    }
    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        // Simulate absent open-order visibility; historical lookup is decisive.
        Ok(Vec::new())
    }
    async fn find_order_by_client_id(
        &self,
        client_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        if self.hide_history.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self
            .accepted
            .lock()
            .unwrap()
            .as_ref()
            .filter(|order| order.client_order_id.as_deref() == Some(client_id))
            .cloned())
    }
    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct MemoryStore {
    orders: Mutex<BTreeMap<String, OrderRecord>>,
    events: Mutex<Vec<String>>,
    fenced: AtomicBool,
}

#[async_trait]
impl RuntimeStore for MemoryStore {
    async fn assert_lease(&self, _: &RuntimeLease) -> Result<(), RuntimeStoreError> {
        if self.fenced.load(Ordering::SeqCst) {
            Err(RuntimeStoreError::FencingLost)
        } else {
            Ok(())
        }
    }
    async fn append_event(
        &self,
        _: &str,
        event_type: &str,
        _: &Value,
        _: i64,
    ) -> Result<i64, RuntimeStoreError> {
        let mut events = self.events.lock().unwrap();
        events.push(event_type.into());
        Ok(events.len() as i64)
    }
    async fn save_order_record(
        &self,
        record: &OrderRecord,
        _: i64,
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
            .filter(|record| record.venue == venue)
            .cloned()
            .collect())
    }
    async fn load_position_states(
        &self,
        _: Venue,
    ) -> Result<Vec<VenuePosition>, RuntimeStoreError> {
        Ok(Vec::new())
    }
    async fn save_position_state(
        &self,
        _: &VenuePosition,
        _: i64,
    ) -> Result<(), RuntimeStoreError> {
        Ok(())
    }
    async fn save_reconcile_report(
        &self,
        _: Venue,
        _: &ReconcileReport,
        _: i64,
    ) -> Result<Uuid, RuntimeStoreError> {
        Ok(Uuid::new_v4())
    }
}

fn rig() -> (
    Arc<MemoryStore>,
    Arc<LostAckVenue>,
    DurableExecution<MemoryStore>,
    OrderIntent,
) {
    let store = Arc::new(MemoryStore::default());
    let venue = Arc::new(LostAckVenue::default());
    let mut registry = AdapterRegistry::default();
    registry.register(Venue::BinancePm, venue.clone());
    let lease = RuntimeLease {
        lease_key: "isolated".into(),
        holder_id: "one".into(),
        fencing_token: 7,
        ttl_seconds: 15,
    };
    let execution = DurableExecution::new(store.clone(), lease, registry);
    let intent = OrderIntent {
        intent_id: Uuid::new_v4(),
        strategy_id: "only-owned-strategy".into(),
        asset: "BTCUSDC".into(),
        venue: Venue::BinancePm,
        side: Side::Buy,
        quantity: Decimal::ONE,
        limit_price: None,
        effect: ExposureEffect::Increase,
        source_signal_id: None,
    };
    (store, venue, execution, intent)
}

#[tokio::test]
async fn exchange_accepts_but_ack_lost_recover_by_identity_without_second_post() {
    let (store, venue, execution, intent) = rig();
    let result = execution.dispatch(&intent).await;
    assert!(matches!(
        result,
        Err(pg_orchestrator::OrchestratorError::Ambiguous { .. })
    ));
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.orders.lock().unwrap()[&intent.client_order_id()].state,
        OrderState::Unknown
    );
    let recovered = execution.recover_ambiguous(Venue::BinancePm).await.unwrap();
    assert_eq!(recovered.items.len(), 1);
    assert!(recovered.items[0].resolved);
    assert_eq!(recovered.items[0].venue_order_id.as_deref(), Some("42"));
    assert_eq!(
        store.orders.lock().unwrap()[&intent.client_order_id()].state,
        OrderState::Open
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert!(
        execution
            .recover_ambiguous(Venue::BinancePm)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn unavailable_order_history_keeps_unknown_and_never_resubmits() {
    let (store, venue, execution, intent) = rig();
    let _ = execution.dispatch(&intent).await;
    venue.hide_history.store(true, Ordering::SeqCst);
    let recovered = execution.recover_ambiguous(Venue::BinancePm).await.unwrap();
    assert_eq!(recovered.items.len(), 1);
    assert!(!recovered.items[0].resolved);
    assert_eq!(
        store.orders.lock().unwrap()[&intent.client_order_id()].state,
        OrderState::Unknown
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_fencing_blocks_order_before_external_side_effect() {
    let (store, venue, execution, intent) = rig();
    store.fenced.store(true, Ordering::SeqCst);
    let result = execution.dispatch(&intent).await;
    assert!(matches!(
        result,
        Err(pg_orchestrator::OrchestratorError::Store(
            RuntimeStoreError::FencingLost
        ))
    ));
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert!(store.orders.lock().unwrap().is_empty());
}
