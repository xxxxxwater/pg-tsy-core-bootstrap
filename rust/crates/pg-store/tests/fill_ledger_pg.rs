//! Run with PG_TEST_DATABASE_URL; CI provides an isolated PostgreSQL service.
use pg_oms::OrderRecord;
use pg_store::{
    PostgresStore,
    fill_ledger::{ExecutionFill, FillInsertOutcome, FillLedgerError},
};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use uuid::Uuid;

#[tokio::test]
async fn fill_is_atomic_idempotent_owned_and_fenced() {
    let Ok(url) = std::env::var("PG_TEST_DATABASE_URL") else {
        eprintln!("PG_TEST_DATABASE_URL unset: PostgreSQL integration test skipped");
        return;
    };
    let store = PostgresStore::connect(&url, 4).await.unwrap();
    store.migrate().await.unwrap();
    let nonce = Uuid::new_v4().to_string();
    let lease = store
        .acquire_lease(&format!("fill-test:{nonce}"), &nonce, 30)
        .await
        .unwrap();
    let intent = OrderIntent {
        intent_id: Uuid::new_v4(),
        strategy_id: "fill-ledger-test".into(),
        asset: "BTCUSDC".into(),
        venue: Venue::BinancePm,
        side: Side::Buy,
        quantity: Decimal::new(10, 3),
        limit_price: None,
        effect: ExposureEffect::Increase,
        source_signal_id: None,
    };
    let mut order = OrderRecord::from_intent(&intent);
    order.venue_order_id = Some("987654".into());
    store
        .save_order_record(&order, lease.fencing_token)
        .await
        .unwrap();
    let fill = ExecutionFill {
        account_scope: format!("test:{nonce}"),
        venue: Venue::BinancePm,
        symbol: "BTCUSDC".into(),
        trade_id: 6001,
        venue_order_id: "987654".into(),
        client_order_id: order.client_order_id.clone(),
        side: Side::Buy,
        quantity: Decimal::new(4, 3),
        price: Decimal::from(80_000),
        commission: Decimal::new(1, 3),
        commission_asset: "USDC".into(),
        realized_pnl: Decimal::new(-2, 2),
        trade_time_ms: 1_700_000_000_000,
    };
    assert_eq!(
        store.insert_execution_fill(&lease, &fill).await.unwrap(),
        FillInsertOutcome::Inserted
    );
    assert_eq!(
        store.insert_execution_fill(&lease, &fill).await.unwrap(),
        FillInsertOutcome::Duplicate
    );
    assert_eq!(
        store
            .recorded_fill_quantity(&lease, &order.client_order_id)
            .await
            .unwrap(),
        Decimal::new(4, 3)
    );

    let mut contradictory = fill.clone();
    contradictory.commission = Decimal::new(2, 3);
    assert!(matches!(
        store.insert_execution_fill(&lease, &contradictory).await,
        Err(FillLedgerError::Conflict)
    ));
    let mut second = fill.clone();
    second.trade_id = 6002;
    second.quantity = Decimal::new(6, 3);
    assert_eq!(
        store.insert_execution_fill(&lease, &second).await.unwrap(),
        FillInsertOutcome::Inserted
    );
    assert_eq!(
        store
            .recorded_fill_quantity(&lease, &order.client_order_id)
            .await
            .unwrap(),
        Decimal::new(10, 3)
    );
    let mut overfill = fill.clone();
    overfill.trade_id = 6003;
    assert!(matches!(
        store.insert_execution_fill(&lease, &overfill).await,
        Err(FillLedgerError::Conflict)
    ));
    let mut manual = fill.clone();
    manual.trade_id = 6004;
    manual.client_order_id = format!("pg{}", "f".repeat(32));
    assert!(matches!(
        store.insert_execution_fill(&lease, &manual).await,
        Err(FillLedgerError::Unowned)
    ));

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_fills WHERE client_order_id = $1")
            .bind(&order.client_order_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event_journal WHERE stream_id = $1 AND event_type = 'order.fill.recorded'")
        .bind(format!("order:{}", order.client_order_id)).fetch_one(store.pool()).await.unwrap();
    assert_eq!(events, 2);
    store.release_lease(&lease).await.unwrap();
    let mut after_lease = fill;
    after_lease.trade_id = 6005;
    assert!(matches!(
        store.insert_execution_fill(&lease, &after_lease).await,
        Err(FillLedgerError::FencingLost)
    ));
}
