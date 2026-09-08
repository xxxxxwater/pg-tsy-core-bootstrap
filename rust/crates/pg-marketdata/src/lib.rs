use pg_types::Venue;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
pub struct Candle {
    pub venue: Venue,
    pub asset: String,
    pub interval_ns: u64,
    pub start_ns: u64,
    pub end_ns: u64,
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
    Candle(Candle),
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
    use rust_decimal::Decimal;

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
}
