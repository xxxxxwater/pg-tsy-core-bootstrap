//! Real PostgreSQL duplicate-dispatch fault injection. No credentials or network.
//! The existing fake-store recovery tests do not exercise duplicate dispatch.
use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenuePositionSnapshot,
};
use pg_oms::{OrderRecord, OrderState};
use pg_orchestrator::{AdapterRegistry, DurableExecution, OrchestratorError, RuntimeStoreError};
use pg_store::PostgresStore;
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

#[derive(Default)]
struct AcceptedButAckLost {
    posts: AtomicUsize,
}

#[async_trait]
impl ExecutionAdapter for AcceptedButAckLost {
    async fn submit(&self, _: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        Err(ExecutionError::Unknown(
            "injected accepted POST / lost ACK".into(),
        ))
    }

    async fn cancel(&self, _: OrderLocator<'_>) -> Result<(), ExecutionError> {
        panic!("duplicate-dispatch test must never cancel an order")
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        Ok(Vec::new())
    }

    async fn find_order_by_client_id(
        &self,
        _: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        Ok(None)
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn duplicate_intent_never_overwrites_unknown_or_sends_second_post() {
    let url = std::env::var("PG_TEST_DATABASE_URL")
        .expect("PG_TEST_DATABASE_URL required: real PostgreSQL test must not silently skip");
    let store = Arc::new(PostgresStore::connect(&url, 5).await.unwrap());
    store.migrate().await.unwrap();
    let nonce = Uuid::new_v4().to_string();
    let lease = store
        .acquire_lease(&format!("duplicate-dispatch:{nonce}"), &nonce, 60)
        .await
        .unwrap();
    let venue = Arc::new(AcceptedButAckLost::default());
    let mut registry = AdapterRegistry::default();
    registry.register(Venue::BinancePm, venue.clone());
    let execution = DurableExecution::new(store.clone(), lease.clone(), registry);
    let intent = OrderIntent {
        intent_id: Uuid::new_v4(),
        strategy_id: "isolated-duplicate-test".into(),
        venue: Venue::BinancePm,
        asset: "BTCUSDC".into(),
        side: Side::Buy,
        quantity: Decimal::ONE,
        limit_price: None,
        effect: ExposureEffect::Increase,
        source_signal_id: None,
    };
    assert!(matches!(
        execution.dispatch(&intent).await,
        Err(OrchestratorError::Ambiguous { .. })
    ));
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert!(matches!(
        execution.dispatch(&intent).await,
        Err(OrchestratorError::Store(RuntimeStoreError::Other(_)))
    ));
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    let orders = store.load_orders_for_venue(Venue::BinancePm).await.unwrap();
    let persisted = orders
        .iter()
        .find(|order| order.client_order_id == intent.client_order_id())
        .unwrap();
    assert_eq!(persisted.state, OrderState::Unknown);
    let intent_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM event_journal WHERE stream_id=$1 AND event_type='order.intent.persisted'",
    )
    .bind(format!("order:{}", intent.client_order_id()))
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(intent_events, 1);

    // A crash between the Created INSERT and dispatch marker must also forbid
    // re-creating that same durable identity, rather than overwriting it.
    let another = OrderIntent {
        intent_id: Uuid::new_v4(),
        ..intent.clone()
    };
    store
        .save_order_record(&OrderRecord::from_intent(&another), lease.fencing_token)
        .await
        .unwrap();
    assert!(matches!(
        execution.dispatch(&another).await,
        Err(OrchestratorError::Store(RuntimeStoreError::Other(_)))
    ));
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    store.release_lease(&lease).await.unwrap();
}
