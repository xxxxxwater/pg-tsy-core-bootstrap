pub mod subscription;
pub mod supervisor_runtime;

pub use subscription::{SubscriptionState, SubscriptionStatus, SubscriptionSupervisor};
pub use supervisor_runtime::SubscriptionRuntime;

use async_trait::async_trait;
use pg_types::Venue;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SequenceError {
    #[error("market data gap: expected {expected}, got {actual}")]
    Gap { expected: u64, actual: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SequenceTracker {
    last: Option<u64>,
}

impl SequenceTracker {
    pub fn apply(&mut self, sequence: u64) -> Result<(), SequenceError> {
        if let Some(last) = self.last {
            let expected = last + 1;
            if sequence != expected {
                return Err(SequenceError::Gap {
                    expected,
                    actual: sequence,
                });
            }
        }
        self.last = Some(sequence);
        Ok(())
    }

    pub fn reset(&mut self, snapshot_sequence: u64) {
        self.last = Some(snapshot_sequence);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggressorSide {
    Buy,
    Sell,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeTick {
    pub venue: Venue,
    pub asset: String,
    pub ts_event_ns: u64,
    pub ts_recv_ns: u64,
    pub price: Decimal,
    pub quantity: Decimal,
    pub aggressor: AggressorSide,
    pub sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BestBidAsk {
    pub venue: Venue,
    pub asset: String,
    pub ts_event_ns: u64,
    pub ts_recv_ns: u64,
    pub bid_price: Decimal,
    pub bid_quantity: Decimal,
    pub ask_price: Decimal,
    pub ask_quantity: Decimal,
    pub sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub quantity: Decimal,
    pub order_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Book {
    pub venue: Venue,
    pub asset: String,
    pub ts_event_ns: u64,
    pub ts_recv_ns: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub sequence: Option<u64>,
    pub is_snapshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub venue: Venue,
    pub asset: String,
    pub interval_ns: u64,
    pub start_ns: u64,
    pub end_ns: u64,
    pub ts_recv_ns: u64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub trades: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarketEvent {
    Trade(TradeTick),
    BestBidAsk(BestBidAsk),
    L2Book(L2Book),
    Candle(Candle),
}

impl MarketEvent {
    pub fn ts_recv_ns(&self) -> u64 {
        match self {
            Self::Trade(event) => event.ts_recv_ns,
            Self::BestBidAsk(event) => event.ts_recv_ns,
            Self::L2Book(event) => event.ts_recv_ns,
            Self::Candle(event) => event.ts_recv_ns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedKind {
    Trades,
    BestBidAsk,
    L2Book,
    Candle { interval_ns: u64 },
}

#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub venue: Venue,
    pub asset: String,
    pub kind: FeedKind,
}

#[derive(Debug, Error)]
pub enum MarketDataError {
    #[error("subscription rejected: {0}")]
    Subscription(String),
    #[error("market data disconnected: {0}")]
    Disconnected(String),
    #[error("market data is stale: last receive {last_recv_ns}, now {now_ns}")]
    Stale { last_recv_ns: u64, now_ns: u64 },
    #[error("market data conversion failed: {0}")]
    Conversion(String),
}

#[async_trait]
pub trait MarketDataSource: Send {
    async fn stream(
        &mut self,
        spec: FeedSpec,
        sink: mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError>;
}

#[derive(Debug, Clone)]
pub struct FeedFreshness {
    max_staleness_ns: u64,
    last_recv_ns: Option<u64>,
}

impl FeedFreshness {
    pub fn new(max_staleness_ns: u64) -> Self {
        assert!(max_staleness_ns > 0, "max staleness must be positive");
        Self {
            max_staleness_ns,
            last_recv_ns: None,
        }
    }

    pub fn observe(&mut self, event: &MarketEvent) {
        self.last_recv_ns = Some(event.ts_recv_ns());
    }

    pub fn ensure_fresh(&self, now_ns: u64) -> Result<(), MarketDataError> {
        let last_recv_ns = self.last_recv_ns.ok_or(MarketDataError::Stale {
            last_recv_ns: 0,
            now_ns,
        })?;
        if now_ns.saturating_sub(last_recv_ns) > self.max_staleness_ns {
            return Err(MarketDataError::Stale {
                last_recv_ns,
                now_ns,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CandleError {
    #[error("candle interval must be non-zero")]
    InvalidInterval,
    #[error("out-of-order trade: current bucket starts at {bucket_start}, got {event_ts}")]
    OutOfOrder { bucket_start: u64, event_ts: u64 },
}

#[derive(Debug, Clone)]
pub struct CandleAggregator {
    interval_ns: u64,
    current: Option<Candle>,
}

impl CandleAggregator {
    pub fn new(interval_ns: u64) -> Result<Self, CandleError> {
        if interval_ns == 0 {
            return Err(CandleError::InvalidInterval);
        }
        Ok(Self {
            interval_ns,
            current: None,
        })
    }

    pub fn push_trade(&mut self, trade: &TradeTick) -> Result<Option<Candle>, CandleError> {
        let bucket_start = trade.ts_event_ns / self.interval_ns * self.interval_ns;
        if let Some(current) = &mut self.current {
            if bucket_start < current.start_ns {
                return Err(CandleError::OutOfOrder {
                    bucket_start: current.start_ns,
                    event_ts: trade.ts_event_ns,
                });
            }
            if bucket_start == current.start_ns {
                current.high = current.high.max(trade.price);
                current.low = current.low.min(trade.price);
                current.close = trade.price;
                current.volume += trade.quantity;
                current.trades += 1;
                current.ts_recv_ns = trade.ts_recv_ns;
                return Ok(None);
            }
        }

        let completed = self.current.take();
        self.current = Some(Candle {
            venue: trade.venue,
            asset: trade.asset.clone(),
            interval_ns: self.interval_ns,
            start_ns: bucket_start,
            end_ns: bucket_start + self.interval_ns,
            ts_recv_ns: trade.ts_recv_ns,
            open: trade.price,
            high: trade.price,
            low: trade.price,
            close: trade.price,
            volume: trade.quantity,
            trades: 1,
        });
        Ok(completed)
    }

    pub fn current(&self) -> Option<&Candle> {
        self.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(ts: u64, px: i64, qty: i64) -> TradeTick {
        TradeTick {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: ts,
            ts_recv_ns: ts,
            price: Decimal::from(px),
            quantity: Decimal::from(qty),
            aggressor: AggressorSide::Buy,
            sequence: None,
        }
    }

    #[test]
    fn aggregates_ticks_into_candles() {
        let mut agg = CandleAggregator::new(1_000).unwrap();
        assert!(agg.push_trade(&trade(100, 10, 2)).unwrap().is_none());
        assert!(agg.push_trade(&trade(900, 12, 3)).unwrap().is_none());
        let completed = agg.push_trade(&trade(1_100, 11, 1)).unwrap().unwrap();
        assert_eq!(completed.open, Decimal::from(10));
        assert_eq!(completed.high, Decimal::from(12));
        assert_eq!(completed.volume, Decimal::from(5));
        assert_eq!(completed.trades, 2);
    }

    #[test]
    fn stale_feed_fails_closed() {
        let event = MarketEvent::Trade(trade(1_000, 10, 1));
        let mut freshness = FeedFreshness::new(100);
        freshness.observe(&event);
        assert!(freshness.ensure_fresh(1_050).is_ok());
        assert!(freshness.ensure_fresh(1_101).is_err());
    }
}
