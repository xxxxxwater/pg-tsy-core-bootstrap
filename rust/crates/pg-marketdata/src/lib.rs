pub mod latest_cache;
pub mod subscription;
pub mod supervisor_runtime;
pub mod universe;

pub use latest_cache::LatestEventCache;
pub use subscription::{SubscriptionState, SubscriptionStatus, SubscriptionSupervisor};
pub use supervisor_runtime::SubscriptionRuntime;
pub use universe::{
    InstrumentDescriptor, ProductType, UniverseError, UniverseFilter, UniverseProvider,
    UniverseSnapshot,
};

use async_trait::async_trait;
use pg_types::{AssetKey, Venue};
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
    pub fn asset_key(&self) -> AssetKey {
        match self {
            Self::Trade(event) => AssetKey::new(event.venue, event.asset.clone()),
            Self::BestBidAsk(event) => AssetKey::new(event.venue, event.asset.clone()),
            Self::L2Book(event) => AssetKey::new(event.venue, event.asset.clone()),
            Self::Candle(event) => AssetKey::new(event.venue, event.asset.clone()),
        }
    }

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

/// Candle resolutions a venue can actually serve, in nanoseconds.
///
/// This is a capability table, not a preference. A definition that asks for a
/// candle stream the venue cannot provide must fail at strategy-load time: a
/// permanently rejected subscription leaves the daemon reconnecting forever and
/// never ready, with no obvious cause.
const HYPERLIQUID_CANDLE_INTERVALS_NS: [u64; 11] = [
    60_000_000_000,
    180_000_000_000,
    300_000_000_000,
    900_000_000_000,
    1_800_000_000_000,
    3_600_000_000_000,
    7_200_000_000_000,
    14_400_000_000_000,
    28_800_000_000_000,
    43_200_000_000_000,
    86_400_000_000_000,
];

/// TWS realtime bars are only requested at the 5 second resolution by this runtime.
const IBKR_CANDLE_INTERVALS_NS: [u64; 1] = [5_000_000_000];

const BINANCE_CANDLE_INTERVALS_NS: [u64; 8] = [
    60_000_000_000,
    180_000_000_000,
    300_000_000_000,
    900_000_000_000,
    1_800_000_000_000,
    3_600_000_000_000,
    14_400_000_000_000,
    86_400_000_000_000,
];

pub fn supported_candle_intervals(venue: Venue) -> &'static [u64] {
    match venue {
        Venue::Hyperliquid => &HYPERLIQUID_CANDLE_INTERVALS_NS,
        Venue::InteractiveBrokers => &IBKR_CANDLE_INTERVALS_NS,
        Venue::BinancePm => &BINANCE_CANDLE_INTERVALS_NS,
    }
}

pub fn candle_interval_supported(venue: Venue, interval_ns: u64) -> bool {
    supported_candle_intervals(venue).contains(&interval_ns)
}

/// The resolution to use when a definition does not pin one explicitly.
///
/// Falls back to the venue's own smallest candle resolution. That is what lets a
/// single template span IBKR (5 second realtime bars) and a crypto venue (1 minute
/// and up) without pretending they share a candle contract.
pub fn default_candle_interval(venue: Venue) -> u64 {
    supported_candle_intervals(venue)
        .first()
        .copied()
        .expect("every supported venue declares at least one candle resolution")
}

/// Render a venue's supported resolutions for an operator-facing error message.
pub fn describe_candle_intervals(venue: Venue) -> String {
    supported_candle_intervals(venue)
        .iter()
        .map(|interval_ns| describe_candle_interval(*interval_ns))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn describe_candle_interval(interval_ns: u64) -> String {
    const SECOND: u64 = 1_000_000_000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if interval_ns.is_multiple_of(DAY) {
        format!("{}d", interval_ns / DAY)
    } else if interval_ns.is_multiple_of(HOUR) {
        format!("{}h", interval_ns / HOUR)
    } else if interval_ns.is_multiple_of(MINUTE) {
        format!("{}m", interval_ns / MINUTE)
    } else {
        format!("{}s", interval_ns / SECOND)
    }
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
    fn hyperliquid_has_no_five_second_candle_stream() {
        // The online factor defaults used to request a 5s candle, which
        // Hyperliquid rejects forever. Keep that impossible combination explicit.
        assert!(!candle_interval_supported(
            Venue::Hyperliquid,
            5_000_000_000
        ));
        assert!(candle_interval_supported(
            Venue::Hyperliquid,
            60_000_000_000
        ));
        assert!(candle_interval_supported(
            Venue::InteractiveBrokers,
            5_000_000_000
        ));
        assert!(!candle_interval_supported(
            Venue::InteractiveBrokers,
            60_000_000_000
        ));
    }

    #[test]
    fn intervals_render_for_error_messages() {
        assert_eq!(describe_candle_interval(60_000_000_000), "1m");
        assert_eq!(describe_candle_interval(5_000_000_000), "5s");
        assert_eq!(describe_candle_interval(86_400_000_000_000), "1d");
        assert_eq!(describe_candle_interval(300_000_000_000), "5m");
        assert!(describe_candle_intervals(Venue::Hyperliquid).contains("1m"));
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
