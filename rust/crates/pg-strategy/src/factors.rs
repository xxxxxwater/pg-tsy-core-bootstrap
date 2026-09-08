use std::collections::VecDeque;

use pg_marketdata::{AggressorSide, MarketEvent};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorConfig {
    pub trade_window: usize,
    pub candle_window: usize,
    pub book_levels: usize,
    pub min_trade_samples: usize,
    pub min_candle_samples: usize,
    pub max_spread_bps: f64,
    pub volatility_soft_cap_bps: f64,
    pub vwap_scale_bps: f64,
    pub momentum_scale_bps: f64,
    pub vwap_weight: f64,
    pub trade_imbalance_weight: f64,
    pub book_imbalance_weight: f64,
    pub momentum_weight: f64,
}

impl Default for FactorConfig {
    fn default() -> Self {
        Self {
            trade_window: 64,
            candle_window: 32,
            book_levels: 5,
            min_trade_samples: 16,
            min_candle_samples: 8,
            max_spread_bps: 25.0,
            volatility_soft_cap_bps: 150.0,
            vwap_scale_bps: 20.0,
            momentum_scale_bps: 35.0,
            vwap_weight: 0.25,
            trade_imbalance_weight: 0.30,
            book_imbalance_weight: 0.25,
            momentum_weight: 0.20,
        }
    }
}

impl FactorConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.trade_window == 0 || self.candle_window == 0 || self.book_levels == 0 {
            return Err("factor windows and book_levels must be positive");
        }
        if self.min_trade_samples == 0 || self.min_trade_samples > self.trade_window {
            return Err("min_trade_samples must be in [1, trade_window]");
        }
        if self.min_candle_samples == 0 || self.min_candle_samples > self.candle_window {
            return Err("min_candle_samples must be in [1, candle_window]");
        }
        if self.max_spread_bps <= 0.0
            || self.volatility_soft_cap_bps <= 0.0
            || self.vwap_scale_bps <= 0.0
            || self.momentum_scale_bps <= 0.0
        {
            return Err("factor scale parameters must be positive");
        }
        if self.vwap_weight.abs()
            + self.trade_imbalance_weight.abs()
            + self.book_imbalance_weight.abs()
            + self.momentum_weight.abs()
            == 0.0
        {
            return Err("at least one directional factor weight must be non-zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorSnapshot {
    pub ts_recv_ns: u64,
    pub last_price: Option<f64>,
    pub vwap: Option<f64>,
    pub vwap_deviation_bps: Option<f64>,
    pub trade_imbalance: Option<f64>,
    pub spread_bps: Option<f64>,
    pub book_imbalance: Option<f64>,
    pub momentum_bps: Option<f64>,
    pub realized_volatility_bps: Option<f64>,
    pub warmup_ratio: f64,
    pub score: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy)]
struct TradeSample {
    price: f64,
    quantity: f64,
    signed_quantity: f64,
}

pub struct RollingFactorEngine {
    config: FactorConfig,
    trades: VecDeque<TradeSample>,
    closes: VecDeque<f64>,
    last_price: Option<f64>,
    bbo: Option<(f64, f64, f64, f64)>,
    book_totals: Option<(f64, f64)>,
    last_recv_ns: u64,
}

impl RollingFactorEngine {
    pub fn new(config: FactorConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            config,
            trades: VecDeque::new(),
            closes: VecDeque::new(),
            last_price: None,
            bbo: None,
            book_totals: None,
            last_recv_ns: 0,
        })
    }

    pub fn config(&self) -> &FactorConfig {
        &self.config
    }

    pub fn on_event(&mut self, event: &MarketEvent) -> Option<FactorSnapshot> {
        self.last_recv_ns = event.ts_recv_ns();
        match event {
            MarketEvent::Trade(trade) => {
                let price = trade.price.to_f64()?;
                let quantity = trade.quantity.to_f64()?;
                if !price.is_finite() || !quantity.is_finite() || price <= 0.0 || quantity <= 0.0 {
                    return None;
                }
                let sign = match trade.aggressor {
                    AggressorSide::Buy => 1.0,
                    AggressorSide::Sell => -1.0,
                    AggressorSide::Unknown => 0.0,
                };
                self.last_price = Some(price);
                self.trades.push_back(TradeSample {
                    price,
                    quantity,
                    signed_quantity: quantity * sign,
                });
                while self.trades.len() > self.config.trade_window {
                    self.trades.pop_front();
                }
            }
            MarketEvent::BestBidAsk(quote) => {
                let bid = quote.bid_price.to_f64()?;
                let bid_qty = quote.bid_quantity.to_f64()?;
                let ask = quote.ask_price.to_f64()?;
                let ask_qty = quote.ask_quantity.to_f64()?;
                if bid > 0.0 && ask >= bid && bid_qty >= 0.0 && ask_qty >= 0.0 {
                    self.bbo = Some((bid, bid_qty, ask, ask_qty));
                    self.last_price = Some((bid + ask) / 2.0);
                }
            }
            MarketEvent::L2Book(book) => {
                let bid_total = book
                    .bids
                    .iter()
                    .take(self.config.book_levels)
                    .filter_map(|level| level.quantity.to_f64())
                    .sum::<f64>();
                let ask_total = book
                    .asks
                    .iter()
                    .take(self.config.book_levels)
                    .filter_map(|level| level.quantity.to_f64())
                    .sum::<f64>();
                if bid_total >= 0.0 && ask_total >= 0.0 {
                    self.book_totals = Some((bid_total, ask_total));
                }
            }
            MarketEvent::Candle(candle) => {
                let close = candle.close.to_f64()?;
                if !close.is_finite() || close <= 0.0 {
                    return None;
                }
                self.last_price = Some(close);
                self.closes.push_back(close);
                while self.closes.len() > self.config.candle_window {
                    self.closes.pop_front();
                }
            }
        }
        Some(self.snapshot())
    }

    pub fn snapshot(&self) -> FactorSnapshot {
        let vwap = self.vwap();
        let vwap_deviation_bps = match (self.last_price, vwap) {
            (Some(price), Some(vwap)) if vwap > 0.0 => Some((price / vwap - 1.0) * 10_000.0),
            _ => None,
        };
        let trade_imbalance = self.trade_imbalance();
        let spread_bps = self.spread_bps();
        let book_imbalance = self.book_imbalance();
        let momentum_bps = self.momentum_bps();
        let realized_volatility_bps = self.realized_volatility_bps();

        let directional = [
            (
                vwap_deviation_bps.map(|value| clip(value / self.config.vwap_scale_bps)),
                self.config.vwap_weight,
            ),
            (trade_imbalance, self.config.trade_imbalance_weight),
            (book_imbalance, self.config.book_imbalance_weight),
            (
                momentum_bps.map(|value| clip(value / self.config.momentum_scale_bps)),
                self.config.momentum_weight,
            ),
        ];

        let configured_weight = directional
            .iter()
            .map(|(_, weight)| weight.abs())
            .sum::<f64>();
        let available_weight = directional
            .iter()
            .filter_map(|(value, weight)| value.map(|_| weight.abs()))
            .sum::<f64>();
        let weighted_sum = directional
            .iter()
            .filter_map(|(value, weight)| value.map(|value| value * *weight))
            .sum::<f64>();
        let score = if available_weight > 0.0 {
            clip(weighted_sum / available_weight)
        } else {
            0.0
        };

        let trade_progress = ratio(self.trades.len(), self.config.min_trade_samples);
        let candle_progress = ratio(self.closes.len(), self.config.min_candle_samples);
        let bbo_progress = if self.bbo.is_some() { 1.0 } else { 0.0 };
        let book_progress = if self.book_totals.is_some() { 1.0 } else { 0.0 };
        let warmup_ratio = (trade_progress + candle_progress + bbo_progress + book_progress) / 4.0;
        let coverage = if configured_weight > 0.0 {
            available_weight / configured_weight
        } else {
            0.0
        };
        let spread_quality = spread_bps
            .map(|spread| 1.0 - (spread / self.config.max_spread_bps).clamp(0.0, 1.0))
            .unwrap_or(0.5);
        let volatility_quality = realized_volatility_bps
            .map(|vol| {
                if vol <= self.config.volatility_soft_cap_bps {
                    1.0
                } else {
                    (self.config.volatility_soft_cap_bps / vol).clamp(0.0, 1.0)
                }
            })
            .unwrap_or(0.75);
        let confidence =
            (warmup_ratio * coverage * spread_quality * volatility_quality).clamp(0.0, 1.0);

        FactorSnapshot {
            ts_recv_ns: self.last_recv_ns,
            last_price: self.last_price,
            vwap,
            vwap_deviation_bps,
            trade_imbalance,
            spread_bps,
            book_imbalance,
            momentum_bps,
            realized_volatility_bps,
            warmup_ratio,
            score,
            confidence,
        }
    }

    fn vwap(&self) -> Option<f64> {
        let notional = self
            .trades
            .iter()
            .map(|trade| trade.price * trade.quantity)
            .sum::<f64>();
        let volume = self.trades.iter().map(|trade| trade.quantity).sum::<f64>();
        (volume > 0.0).then_some(notional / volume)
    }

    fn trade_imbalance(&self) -> Option<f64> {
        let signed = self
            .trades
            .iter()
            .map(|trade| trade.signed_quantity)
            .sum::<f64>();
        let total = self.trades.iter().map(|trade| trade.quantity).sum::<f64>();
        (total > 0.0).then_some((signed / total).clamp(-1.0, 1.0))
    }

    fn spread_bps(&self) -> Option<f64> {
        let (bid, _, ask, _) = self.bbo?;
        let mid = (bid + ask) / 2.0;
        (mid > 0.0).then_some((ask - bid) / mid * 10_000.0)
    }

    fn book_imbalance(&self) -> Option<f64> {
        let (bid, ask) = self.book_totals?;
        let total = bid + ask;
        (total > 0.0).then_some(((bid - ask) / total).clamp(-1.0, 1.0))
    }

    fn momentum_bps(&self) -> Option<f64> {
        let first = *self.closes.front()?;
        let last = *self.closes.back()?;
        (first > 0.0).then_some((last / first - 1.0) * 10_000.0)
    }

    fn realized_volatility_bps(&self) -> Option<f64> {
        if self.closes.len() < 2 {
            return None;
        }
        let returns = self
            .closes
            .iter()
            .zip(self.closes.iter().skip(1))
            .filter_map(|(left, right)| {
                (*left > 0.0 && *right > 0.0).then_some((right / left).ln())
            })
            .collect::<Vec<_>>();
        if returns.is_empty() {
            return None;
        }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns
            .iter()
            .map(|value| {
                let diff = value - mean;
                diff * diff
            })
            .sum::<f64>()
            / returns.len() as f64;
        Some(variance.sqrt() * 10_000.0)
    }
}

fn ratio(current: usize, target: usize) -> f64 {
    (current as f64 / target as f64).clamp(0.0, 1.0)
}

fn clip(value: f64) -> f64 {
    value.clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_marketdata::{BestBidAsk, BookLevel, Candle, L2Book, TradeTick};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    fn config() -> FactorConfig {
        FactorConfig {
            trade_window: 4,
            candle_window: 3,
            book_levels: 2,
            min_trade_samples: 2,
            min_candle_samples: 2,
            max_spread_bps: 200.0,
            volatility_soft_cap_bps: 500.0,
            vwap_scale_bps: 100.0,
            momentum_scale_bps: 100.0,
            vwap_weight: 0.25,
            trade_imbalance_weight: 0.35,
            book_imbalance_weight: 0.25,
            momentum_weight: 0.15,
        }
    }

    #[test]
    fn computes_directional_microstructure_factors() {
        let mut engine = RollingFactorEngine::new(config()).unwrap();
        let quote = MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: 1,
            ts_recv_ns: 1,
            bid_price: Decimal::from(100),
            bid_quantity: Decimal::from(10),
            ask_price: Decimal::from(101),
            ask_quantity: Decimal::from(5),
            sequence: None,
        });
        engine.on_event(&quote);
        let book = MarketEvent::L2Book(L2Book {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: 2,
            ts_recv_ns: 2,
            bids: vec![BookLevel {
                price: Decimal::from(100),
                quantity: Decimal::from(20),
                order_count: None,
            }],
            asks: vec![BookLevel {
                price: Decimal::from(101),
                quantity: Decimal::from(5),
                order_count: None,
            }],
            sequence: None,
            is_snapshot: true,
        });
        engine.on_event(&book);
        for (ts, px) in [(3, 101), (4, 102)] {
            engine.on_event(&MarketEvent::Trade(TradeTick {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: ts,
                ts_recv_ns: ts,
                price: Decimal::from(px),
                quantity: Decimal::ONE,
                aggressor: AggressorSide::Buy,
                sequence: None,
            }));
        }
        for (ts, close) in [(5, 101), (6, 103)] {
            engine.on_event(&MarketEvent::Candle(Candle {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                interval_ns: 1_000,
                start_ns: ts,
                end_ns: ts + 1_000,
                ts_recv_ns: ts,
                open: Decimal::from(close),
                high: Decimal::from(close),
                low: Decimal::from(close),
                close: Decimal::from(close),
                volume: Decimal::ONE,
                trades: 1,
            }));
        }
        let snapshot = engine.snapshot();
        assert!(snapshot.trade_imbalance.unwrap() > 0.9);
        assert!(snapshot.book_imbalance.unwrap() > 0.5);
        assert!(snapshot.momentum_bps.unwrap() > 0.0);
        assert!(snapshot.score > 0.0);
        assert!(snapshot.confidence > 0.0);
    }

    #[test]
    fn spread_at_or_above_limit_suppresses_confidence() {
        let mut engine = RollingFactorEngine::new(FactorConfig {
            max_spread_bps: 50.0,
            ..config()
        })
        .unwrap();
        engine.on_event(&MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: 1,
            ts_recv_ns: 1,
            bid_price: Decimal::from(100),
            bid_quantity: Decimal::ONE,
            ask_price: Decimal::from(101),
            ask_quantity: Decimal::ONE,
            sequence: None,
        }));
        assert_eq!(engine.snapshot().confidence, 0.0);
    }
}
