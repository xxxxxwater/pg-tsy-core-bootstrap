use pg_sim::{
    MarketPhase, MarketSnapshot, MatchingEngine, OrderKind, SimOrder, SimSide, TimeInForce,
};
use rust_decimal::Decimal;
use std::time::Instant;
use uuid::Uuid;

fn main() {
    let iterations = 100_000u64;
    let market = MarketSnapshot {
        bid_price: Decimal::from(99),
        bid_quantity: Decimal::from(10),
        ask_price: Decimal::from(100),
        ask_quantity: Decimal::from(10),
        phase: MarketPhase::Continuous,
        now_ns: 1,
    };
    let started = Instant::now();
    for _ in 0..iterations {
        let mut engine = MatchingEngine::default();
        let order = SimOrder {
            order_id: Uuid::new_v4(),
            side: SimSide::Buy,
            kind: OrderKind::Limit,
            quantity: Decimal::ONE,
            limit_price: Some(Decimal::from(101)),
            time_in_force: TimeInForce::Ioc,
            expire_at_ns: None,
            post_only: false,
            reduce_only: false,
            display_quantity: None,
            contingency: None,
        };
        let id = order.order_id;
        engine.submit(order, true).expect("submit");
        engine.match_once(id, market, Decimal::ZERO).expect("match");
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "pg-sim throughput smoke: iterations={} elapsed_s={:.6} interactions_per_second={:.0}",
        iterations,
        elapsed,
        iterations as f64 / elapsed
    );
}
