//! Real PostgreSQL integration; the separate CI workflow must set PG_TEST_DATABASE_URL.
use pg_oms::{OrderRecord, OrderState};
use pg_store::{
    PostgresStore,
    fill_ledger::{ExecutionFill, FillLedgerError, settlement::CompleteOrderHistory},
};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use uuid::Uuid;

#[tokio::test]
async fn complete_history_updates_oms_and_fills_atomically_without_reopening() {
    let Ok(url) = std::env::var("PG_TEST_DATABASE_URL") else {
        eprintln!("PG_TEST_DATABASE_URL unset: settlement test skipped");
        return;
    };
    let store = PostgresStore::connect(&url, 4).await.unwrap();
    store.migrate().await.unwrap();
    let nonce = Uuid::new_v4().to_string();
    let lease = store
        .acquire_lease(&format!("settlement:{nonce}"), &nonce, 60)
        .await
        .unwrap();
    let intent = OrderIntent {
        intent_id: Uuid::new_v4(),
        strategy_id: "settlement-only-owned".into(),
        asset: "BTCUSDC".into(),
        venue: Venue::BinancePm,
        side: Side::Buy,
        quantity: Decimal::new(10, 3),
        limit_price: None,
        effect: ExposureEffect::Increase,
        source_signal_id: None,
    };
    let mut order = OrderRecord::from_intent(&intent);
    order.venue_order_id = Some("123".into());
    order.state = OrderState::Open;
    store.save_order_record(&order, lease.fencing_token).await.unwrap();
    let first = ExecutionFill {
        account_scope: format!("test:{nonce}"),
        venue: Venue::BinancePm,
        symbol: "BTCUSDC".into(),
        trade_id: 9001,
        venue_order_id: "123".into(),
        client_order_id: order.client_order_id.clone(),
        side: Side::Buy,
        quantity: Decimal::new(4, 3),
        price: Decimal::from(80_000),
        commission: Decimal::new(1, 3),
        commission_asset: "USDC".into(),
        realized_pnl: Decimal::ZERO,
        trade_time_ms: 1_700_000_000_000,
    };
    let mut history = CompleteOrderHistory {
        account_scope: first.account_scope.clone(),
        client_order_id: order.client_order_id.clone(),
        venue_order_id: "123".into(),
        authoritative_filled: Decimal::new(4, 3),
        authoritative_state: OrderState::PartiallyFilled,
        trades: vec![first.clone()],
    };
    let first_result = store.settle_complete_order_history(&lease, &history).await.unwrap();
    assert_eq!(first_result.inserted_trades, 1);
    assert_eq!(first_result.filled_quantity, Decimal::new(4, 3));
    let state: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM order_records WHERE client_order_id=$1",
    )
    .bind(&order.client_order_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let observed: OrderRecord = serde_json::from_value(state).unwrap();
    assert_eq!(observed.state, OrderState::PartiallyFilled);
    assert_eq!(observed.filled_quantity, Decimal::new(4, 3));
    let replay = store.settle_complete_order_history(&lease, &history).await.unwrap();
    assert_eq!(replay.inserted_trades, 0);

    let mut tampered = history.clone();
    tampered.trades[0].commission = Decimal::new(2, 3);
    assert!(matches!(
        store.settle_complete_order_history(&lease, &tampered).await,
        Err(FillLedgerError::Conflict)
    ));
    let mut second = first.clone();
    second.trade_id = 9002;
    second.quantity = Decimal::new(6, 3);
    history.trades.push(second);
    history.authoritative_state = OrderState::Filled;
    history.authoritative_filled = Decimal::new(10, 3);
    let finished = store.settle_complete_order_history(&lease, &history).await.unwrap();
    assert_eq!(finished.inserted_trades, 1);
    assert_eq!(finished.filled_quantity, Decimal::new(10, 3));
    let total = store.recorded_fill_quantity(&lease, &order.client_order_id).await.unwrap();
    assert_eq!(total, Decimal::new(10, 3));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_fills WHERE client_order_id=$1",
    )
    .bind(&order.client_order_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count, 2);
    let journal_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM event_journal WHERE stream_id=$1 AND event_type='order.fill.recorded'",
    )
    .bind(format!("order:{}", order.client_order_id))
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(journal_count, 2);

    // Replaying only the first trade cannot reduce or reopen a terminal order.
    history.trades.pop();
    history.authoritative_state = OrderState::PartiallyFilled;
    history.authoritative_filled = Decimal::new(4, 3);
    assert!(matches!(
        store.settle_complete_order_history(&lease, &history).await,
        Err(FillLedgerError::Conflict)
    ));
    let state: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM order_records WHERE client_order_id=$1",
    )
    .bind(&order.client_order_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let observed: OrderRecord = serde_json::from_value(state).unwrap();
    assert_eq!(observed.state, OrderState::Filled);
    assert_eq!(observed.filled_quantity, Decimal::new(10, 3));
    store.release_lease(&lease).await.unwrap();
    assert!(matches!(
        store.settle_complete_order_history(&lease, &history).await,
        Err(FillLedgerError::FencingLost)
    ));
}
