//! Opt-in REAL Binance public-network probe; never submits orders or uses API keys.
//!
//! Run explicitly:
//! cargo test --locked -p pg-binance --test live_public -- --ignored --nocapture
//! This is evidence for the public Rust transport only, not a signed/private
//! execution or unattended trading acceptance test. No production daemon runs.

use std::time::Duration;

use pg_binance::market_transport::BinanceMarketDataSource;
use pg_marketdata::{FeedKind, FeedSpec, MarketDataSource, MarketEvent};
use pg_types::Venue;
use tokio::{sync::mpsc, time::timeout};

/// Drop one live connection after the first valid event. Reconnecting MUST use
/// a fresh source and (for depth) a fresh snapshot/sequence bridge.
async fn first_real_event(kind: FeedKind) -> MarketEvent {
    let (sender, mut receiver) = mpsc::channel(64);
    let mut source = BinanceMarketDataSource::new().expect("construct public TLS client");
    let worker = tokio::spawn(async move {
        source
            .stream(
                FeedSpec {
                    venue: Venue::BinancePm,
                    asset: "BTCUSDC".into(),
                    kind,
                },
                sender,
            )
            .await
    });
    let event = match timeout(Duration::from_secs(20), receiver.recv()).await {
        Ok(Some(event)) => event,
        Ok(None) => panic!("public transport terminated before first event: {:?}", worker.await),
        Err(_) => {
            worker.abort();
            panic!("timed out waiting for REAL public venue event");
        }
    };
    worker.abort();
    assert!(worker.await.expect_err("public socket must be canceled").is_cancelled());
    event
}

#[tokio::test]
#[ignore = "requires live public Binance internet; never uses account credentials or orders"]
async fn public_websocket_reconnect_and_rest_depth_bridge() {
    let first = first_real_event(FeedKind::BestBidAsk).await;
    let second = first_real_event(FeedKind::BestBidAsk).await;
    for (index, event) in [first, second].into_iter().enumerate() {
        let MarketEvent::BestBidAsk(bbo) = event else {
            panic!("expected genuine Binance best bid/ask");
        };
        assert_eq!(bbo.asset, "BTCUSDC");
        assert_eq!(bbo.venue, Venue::BinancePm);
        assert!(bbo.ts_event_ns > 0 && bbo.ts_recv_ns > 0);
        assert!(bbo.bid_price > rust_decimal::Decimal::ZERO);
        assert!(bbo.bid_price < bbo.ask_price);
        println!(
            "PUBLIC_WS_RECONNECT_EVIDENCE session={} venue=BinancePm symbol=BTCUSDC bid_lt_ask=true event_ns={}",
            index + 1,
            bbo.ts_event_ns
        );
    }

    // Report geo/IP/network restrictions explicitly instead of misdiagnosing
    // them as a local L2 sequence bug. No account endpoints or API keys.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = http
        .get("https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDC&limit=1000")
        .send()
        .await
        .expect("public depth HTTP transport");
    println!(
        "PUBLIC_DEPTH_REST_DIAGNOSTIC status={} content_length={:?}",
        response.status(),
        response.content_length()
    );
    assert!(
        response.status().is_success(),
        "Binance public depth REST is blocked from this runner: {}",
        response.status()
    );

    // This cannot publish from a REST snapshot alone. Reaching this event
    // means the real WS delta passed the real REST snapshot sequence bridge.
    let event = first_real_event(FeedKind::L2Book).await;
    let MarketEvent::L2Book(book) = event else {
        panic!("expected genuinely bridged Binance L2 book");
    };
    assert_eq!(book.asset, "BTCUSDC");
    assert_eq!(book.venue, Venue::BinancePm);
    assert!(!book.bids.is_empty() && !book.asks.is_empty());
    assert!(book.bids[0].price < book.asks[0].price);
    assert!(book.sequence.is_some());
    println!(
        "PUBLIC_REST_WS_L2_EVIDENCE symbol=BTCUSDC bridged=true sequence={:?} bids={} asks={}",
        book.sequence,
        book.bids.len(),
        book.asks.len()
    );
}
