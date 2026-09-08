use std::collections::VecDeque;

use pg_marketdata::MarketEvent;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::factors::FactorSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EntryFilterConfig {
    pub enabled: bool,
    pub volume_window: usize,
    pub min_volume_samples: usize,
    pub min_momentum_bps: Option<f64>,
    pub min_volume_ratio: Option<f64>,
    pub min_trade_imbalance: Option<f64>,
    pub min_book_imbalance: Option<f64>,
    pub max_spread_bps: Option<f64>,
    pub max_realized_volatility_bps: Option<f64>,
}

impl Default for EntryFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            volume_window: 24,
            min_volume_samples: 6,
            min_momentum_bps: None,
            min_volume_ratio: None,
            min_trade_imbalance: None,
            min_book_imbalance: None,
            max_spread_bps: None,
            max_realized_volatility_bps: None,
        }
    }
}

impl EntryFilterConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.volume_window < 2 {
            return Err("entry filter volume_window must be >= 2");
        }
        if self.min_volume_samples < 2 || self.min_volume_samples > self.volume_window {
            return Err("min_volume_samples must be in [2, volume_window]");
        }
        if self.min_volume_ratio.is_some_and(|value| value <= 0.0) {
            return Err("min_volume_ratio must be positive");
        }
        if self.max_spread_bps.is_some_and(|value| value <= 0.0) {
            return Err("max_spread_bps must be positive");
        }
        if self
            .max_realized_volatility_bps
            .is_some_and(|value| value <= 0.0)
        {
            return Err("max_realized_volatility_bps must be positive");
        }
        if self
            .min_trade_imbalance
            .is_some_and(|value| !(-1.0..=1.0).contains(&value))
            || self
                .min_book_imbalance
                .is_some_and(|value| !(-1.0..=1.0).contains(&value))
        {
            return Err("imbalance thresholds must be in [-1, 1]");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFilterSnapshot {
    pub allowed: bool,
    pub volume_ratio: Option<f64>,
    pub reasons: Vec<String>,
}

pub struct MomentumVolumeSelector {
    config: EntryFilterConfig,
    volumes: VecDeque<f64>,
}

impl MomentumVolumeSelector {
    pub fn new(config: EntryFilterConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            config,
            volumes: VecDeque::new(),
        })
    }

    pub fn config(&self) -> &EntryFilterConfig {
        &self.config
    }

    pub fn observe(&mut self, event: &MarketEvent) {
        let MarketEvent::Candle(candle) = event else {
            return;
        };
        let Some(volume) = candle.volume.to_f64() else {
            return;
        };
        if !volume.is_finite() || volume < 0.0 {
            return;
        }
        self.volumes.push_back(volume);
        while self.volumes.len() > self.config.volume_window {
            self.volumes.pop_front();
        }
    }

    pub fn evaluate(&self, factors: &FactorSnapshot) -> EntryFilterSnapshot {
        if !self.config.enabled {
            return EntryFilterSnapshot {
                allowed: true,
                volume_ratio: self.volume_ratio(),
                reasons: Vec::new(),
            };
        }

        let volume_ratio = self.volume_ratio();
        let mut reasons = Vec::new();

        require_min(
            "momentum_bps",
            factors.momentum_bps,
            self.config.min_momentum_bps,
            &mut reasons,
        );
        require_min(
            "volume_ratio",
            volume_ratio,
            self.config.min_volume_ratio,
            &mut reasons,
        );
        require_min(
            "trade_imbalance",
            factors.trade_imbalance,
            self.config.min_trade_imbalance,
            &mut reasons,
        );
        require_min(
            "book_imbalance",
            factors.book_imbalance,
            self.config.min_book_imbalance,
            &mut reasons,
        );
        require_max(
            "spread_bps",
            factors.spread_bps,
            self.config.max_spread_bps,
            &mut reasons,
        );
        require_max(
            "realized_volatility_bps",
            factors.realized_volatility_bps,
            self.config.max_realized_volatility_bps,
            &mut reasons,
        );

        EntryFilterSnapshot {
            allowed: reasons.is_empty(),
            volume_ratio,
            reasons,
        }
    }

    pub fn volume_ratio(&self) -> Option<f64> {
        if self.volumes.len() < self.config.min_volume_samples {
            return None;
        }
        let latest = *self.volumes.back()?;
        let baseline_count = self.volumes.len().saturating_sub(1);
        if baseline_count == 0 {
            return None;
        }
        let baseline = self
            .volumes
            .iter()
            .take(baseline_count)
            .copied()
            .sum::<f64>()
            / baseline_count as f64;
        (baseline > 0.0).then_some(latest / baseline)
    }
}

fn require_min(name: &str, actual: Option<f64>, threshold: Option<f64>, reasons: &mut Vec<String>) {
    let Some(threshold) = threshold else {
        return;
    };
    match actual {
        Some(actual) if actual >= threshold => {}
        Some(actual) => reasons.push(format!("{name} {actual:.6} < minimum {threshold:.6}")),
        None => reasons.push(format!("{name} unavailable")),
    }
}

fn require_max(name: &str, actual: Option<f64>, threshold: Option<f64>, reasons: &mut Vec<String>) {
    let Some(threshold) = threshold else {
        return;
    };
    match actual {
        Some(actual) if actual <= threshold => {}
        Some(actual) => reasons.push(format!("{name} {actual:.6} > maximum {threshold:.6}")),
        None => reasons.push(format!("{name} unavailable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_marketdata::Candle;
    use pg_types::Venue;
    use rust_decimal::Decimal;

    fn candle(ts: u64, close: i64, volume: i64) -> MarketEvent {
        MarketEvent::Candle(Candle {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            interval_ns: 300_000_000_000,
            start_ns: ts,
            end_ns: ts + 1,
            ts_recv_ns: ts,
            open: Decimal::from(close),
            high: Decimal::from(close),
            low: Decimal::from(close),
            close: Decimal::from(close),
            volume: Decimal::from(volume),
            trades: 1,
        })
    }

    fn factors() -> FactorSnapshot {
        FactorSnapshot {
            ts_recv_ns: 1,
            last_price: Some(100.0),
            vwap: Some(99.0),
            vwap_deviation_bps: Some(100.0),
            trade_imbalance: Some(0.4),
            spread_bps: Some(5.0),
            book_imbalance: Some(0.2),
            momentum_bps: Some(80.0),
            realized_volatility_bps: Some(40.0),
            warmup_ratio: 1.0,
            score: 0.8,
            confidence: 0.9,
        }
    }

    #[test]
    fn momentum_volume_filter_waits_for_volume_warmup() {
        let config = EntryFilterConfig {
            enabled: true,
            volume_window: 4,
            min_volume_samples: 3,
            min_momentum_bps: Some(50.0),
            min_volume_ratio: Some(1.5),
            ..EntryFilterConfig::default()
        };
        let mut selector = MomentumVolumeSelector::new(config).unwrap();
        selector.observe(&candle(1, 100, 10));
        selector.observe(&candle(2, 101, 10));
        let warmup = selector.evaluate(&factors());
        assert!(!warmup.allowed);
        assert!(
            warmup
                .reasons
                .iter()
                .any(|reason| reason.contains("volume_ratio unavailable"))
        );

        selector.observe(&candle(3, 102, 30));
        let selected = selector.evaluate(&factors());
        assert!(selected.allowed);
        assert_eq!(selected.volume_ratio, Some(3.0));
    }
}
