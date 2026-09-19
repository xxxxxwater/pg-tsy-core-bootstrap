//! Runtime-level regression: compare durable truth BEFORE accepting a venue snapshot.
//! No credentials, network calls or production trading adapters.
use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenueOrderState, VenuePositionSnapshot,
};
use pg_oms::{OrderEvent, OrderRecord, OrderState};
use pg_orchestrator::{AdapterRegistry, DurableExecution, RuntimeStore, RuntimeStoreError};
use pg_reconcile::{Ownership, ReconcileReport, VenuePosition};
use pg_store::RuntimeLease;
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

struct FakeAdapter {
    remote: Mutex<Option<VenueOrderSnapshot>>,
}

#[async_trait]
impl ExecutionAdapter for FakeAdapter {
    async fn submit(&self, _: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        panic!("reconciliation must never submit an order")
    }
    async fn cancel(&self, _: OrderLocator<'_>) -> Result<(), ExecutionError> {
        panic!("reconciliation must never cancel an order")
    }
    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        Ok(self.remote.lock().unwrap().iter().cloned().collect())
    }
    async fn find_order_by_client_id(
        &self,
        client_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        Ok(self
            .remote
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

struct MemoryStore {
    order: Mutex<OrderRecord>,
    report: Mutex<Option<ReconcileReport>>,
    event_types: Mutex<Vec<String>>,
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
        let mut events = self.event_types.lock().unwrap();
        events.push(event_type.to_owned());
        Ok(events.len() as i64)
    }
    async fn save_order_record(
        &self,
        record: &OrderRecord,
        _: i64,
    ) -> Result<(), RuntimeStoreError> {
        *self.order.lock().unwrap() = record.clone();
        Ok(())
    }
    async fn load_orders_for_venue(&self, _: Venue) -> Result<Vec<OrderRecord>, RuntimeStoreError> {
        Ok(vec![self.order.lock().unwrap().clone()])
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
        report: &ReconcileReport,
        _: i64,
    ) -> Result<Uuid, RuntimeStoreError> {
        *self.report.lock().unwrap() = Some(report.clone());
        Ok(Uuid::new_v4())
    }
}

fn fixture() -> (
    Arc<MemoryStore>,
    Arc<FakeAdapter>,
    DurableExecution<MemoryStore>,
) {
    let intent = OrderIntent {
        intent_id: Uuid::new_v4(),
        strategy_id: "test-owner".into(),
        asset: "BTCUSDC".into(),
        venue: Venue::BinancePm,
        side: Side::Buy,
        quantity: Decimal::from(100),
        limit_price: None,
        effect: ExposureEffect::Increase,
        source_signal_id: None,
    };
    let mut local = OrderRecord::from_intent(&intent);
    local.apply(OrderEvent::SubmitRequested).unwrap();
    local.accept("42").unwrap();
    let remote = VenueOrderSnapshot {
        venue_order_id: "42".into(),
        client_order_id: Some(local.client_order_id.clone()),
        asset: "BTCUSDC".into(),
        side: Side::Buy,
        requested_quantity: Decimal::from(100),
        filled_quantity: Decimal::ZERO,
        limit_price: None,
        state: VenueOrderState::Open,
    };
    let store = Arc::new(MemoryStore {
        order: Mutex::new(local),
        report: Mutex::new(None),
        event_types: Mutex::new(Vec::new()),
        fenced: AtomicBool::new(false),
    });
    let adapter = Arc::new(FakeAdapter {
        remote: Mutex::new(Some(remote)),
    });
    let mut adapters = AdapterRegistry::default();
    adapters.register(Venue::BinancePm, adapter.clone());
    let lease = RuntimeLease {
        lease_key: "isolated-reconcile-test".into(),
        holder_id: "test-runner".into(),
        fencing_token: 7,
        ttl_seconds: 15,
    };
    let execution = DurableExecution::new(store.clone(), lease, adapters);
    (store, adapter, execution)
}

#[tokio::test]
async fn original_fill_mismatch_survives_runtime_reconcile() {
    let (store, adapter, execution) = fixture();
    store
        .order
        .lock()
        .unwrap()
        .apply_fill(Decimal::from(5))
        .unwrap();
    {
        let mut remote = adapter.remote.lock().unwrap();
        let snapshot = remote.as_mut().unwrap();
        snapshot.filled_quantity = Decimal::from(6);
        snapshot.state = VenueOrderState::PartiallyFilled;
    }
    let cycle = execution.reconcile_once(Venue::BinancePm).await.unwrap();
    assert!(cycle.report.blocks(Venue::BinancePm, "BTCUSDC"));
    assert_eq!(
        store.order.lock().unwrap().filled_quantity,
        Decimal::from(5)
    );
    assert_eq!(cycle.orders[0].filled_quantity, Decimal::from(5));
    assert!(!store.report.lock().unwrap().as_ref().unwrap().clean());
}

#[tokio::test]
async fn venue_order_id_mismatch_is_not_overwritten() {
    let (store, adapter, execution) = fixture();
    adapter
        .remote
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .venue_order_id = "999".into();
    let cycle = execution.reconcile_once(Venue::BinancePm).await.unwrap();
    assert!(cycle.report.blocks(Venue::BinancePm, "BTCUSDC"));
    assert_eq!(
        store.order.lock().unwrap().venue_order_id.as_deref(),
        Some("42")
    );
}

#[tokio::test]
async fn identity_mismatch_never_adopts_wrong_asset_side_or_size() {
    for case in 0..4 {
        let (store, adapter, execution) = fixture();
        match case {
            0 => adapter.remote.lock().unwrap().as_mut().unwrap().asset = "ETHUSDC".into(),
            1 => adapter.remote.lock().unwrap().as_mut().unwrap().side = Side::Sell,
            2 => {
                adapter
                    .remote
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .requested_quantity = Decimal::from(101)
            }
            _ => store.order.lock().unwrap().side = None,
        }
        let original = store.order.lock().unwrap().clone();
        let cycle = execution.reconcile_once(Venue::BinancePm).await.unwrap();
        assert!(
            cycle.report.blocks(Venue::BinancePm, "BTCUSDC"),
            "case {case}"
        );
        assert_eq!(
            store.order.lock().unwrap().venue_order_id,
            original.venue_order_id
        );
        assert_eq!(store.order.lock().unwrap().state, original.state);
        assert_eq!(store.order.lock().unwrap().side, original.side);
    }
}

#[tokio::test]
async fn terminal_local_must_not_resurrect_from_remote_open() {
    let (store, _, execution) = fixture();
    store.order.lock().unwrap().state = OrderState::Canceled;
    let cycle = execution.reconcile_once(Venue::BinancePm).await.unwrap();
    assert!(cycle.report.blocks(Venue::BinancePm, "BTCUSDC"));
    assert_eq!(store.order.lock().unwrap().state, OrderState::Canceled);
}

#[tokio::test]
async fn fencing_loss_prevents_any_mutation_or_venue_calls() {
    let (store, _, execution) = fixture();
    store.fenced.store(true, Ordering::SeqCst);
    let result = execution.reconcile_once(Venue::BinancePm).await;
    assert!(matches!(
        result,
        Err(pg_orchestrator::OrchestratorError::Store(
            RuntimeStoreError::FencingLost
        ))
    ));
    assert!(store.report.lock().unwrap().is_none());
    assert!(store.event_types.lock().unwrap().is_empty());
}

#[test]
fn manual_ownership_remains_distinct() {
    assert_ne!(Ownership::Manual, Ownership::Strategy("test-owner".into()));
}
