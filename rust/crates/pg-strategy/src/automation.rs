use pg_marketdata::{FeedKind, FeedSpec, MarketEvent};
use pg_types::{Signal, Venue};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::factors::{FactorConfig, FactorSnapshot, RollingFactorEngine};
use crate::selector::{EntryFilterConfig, EntryFilterSnapshot, MomentumVolumeSelector};
use crate::{StrategyConfig, StrategyDecision, StrategyMachine, StrategyPhase};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyAutomationConfig {
    pub factors: FactorConfig,
    pub candle_interval_ns: u64,
    pub signal_horizon_ms: u64,
    pub signal_ttl_ms: u64,
    pub min_confidence: f64,
    pub min_emit_interval_ns: u64,
    pub alpha_id: String,
    #[serde(default)]
    pub entry_filter: EntryFilterConfig,
}

impl Default for StrategyAutomationConfig {
    fn default() -> Self {
        Self {
            factors: FactorConfig::default(),
            candle_interval_ns: 5_000_000_000,
            signal_horizon_ms: 5_000,
            signal_ttl_ms: 2_000,
            min_confidence: 0.45,
            min_emit_interval_ns: 250_000_000,
            alpha_id: "alpha.live.microstructure.v1".into(),
            entry_filter: EntryFilterConfig::default(),
        }
    }
}

impl StrategyAutomationConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.factors.validate()?;
        self.entry_filter.validate()?;
        if self.candle_interval_ns == 0 {
            return Err("candle_interval_ns must be positive");
        }
        if self.signal_horizon_ms == 0 || self.signal_ttl_ms == 0 {
            return Err("signal horizon and ttl must be positive");
        }
        if !(0.0..=1.0).contains(&self.min_confidence) {
            return Err("min_confidence must be in [0, 1]");
        }
        if self.alpha_id.trim().is_empty() {
            return Err("alpha_id must not be empty");
        }
        Ok(())
    }

    pub fn subscriptions(&self, strategy: &StrategyConfig) -> Vec<FeedSpec> {
        [
            FeedKind::Trades,
            FeedKind::BestBidAsk,
            FeedKind::L2Book,
            FeedKind::Candle {
                interval_ns: self.candle_interval_ns,
            },
        ]
        .into_iter()
        .map(|kind| FeedSpec {
            venue: strategy.venue,
            asset: strategy.asset.clone(),
            kind,
        })
        .collect()
    }
}

pub struct AutomationOutput {
    pub factors: Option<FactorSnapshot>,
    pub entry_filter: Option<EntryFilterSnapshot>,
    pub signal: Option<Signal>,
    pub decision: StrategyDecision,
}

impl AutomationOutput {
    fn idle(factors: Option<FactorSnapshot>, entry_filter: Option<EntryFilterSnapshot>) -> Self {
        Self {
            factors,
            entry_filter,
            signal: None,
            decision: StrategyDecision::Noop,
        }
    }
}

pub struct AutomatedStrategy {
    pub machine: StrategyMachine,
    config: StrategyAutomationConfig,
    factors: RollingFactorEngine,
    selector: MomentumVolumeSelector,
    last_emit_ns: Option<u64>,
    emission_sequence: u64,
}

impl AutomatedStrategy {
    pub fn new(
        strategy: StrategyConfig,
        config: StrategyAutomationConfig,
    ) -> Result<Self, &'static str> {
        config.validate()?;
        let factors = RollingFactorEngine::new(config.factors.clone())?;
        let selector = MomentumVolumeSelector::new(config.entry_filter.clone())?;
        Ok(Self {
            machine: StrategyMachine::new(strategy)?,
            config,
            factors,
            selector,
            last_emit_ns: None,
            emission_sequence: 0,
        })
    }

    pub fn subscriptions(&self) -> Vec<FeedSpec> {
        self.config.subscriptions(&self.machine.config)
    }

    pub fn on_market_event(&mut self, event: &MarketEvent) -> AutomationOutput {
        let (venue, asset, now_ns) = event_scope(event);
        if venue != self.machine.config.venue || asset != self.machine.config.asset {
            return AutomationOutput::idle(None, None);
        }

        self.selector.observe(event);
        let snapshot = self.factors.on_event(event);
        let Some(snapshot) = snapshot else {
            return AutomationOutput::idle(None, None);
        };
        if snapshot.confidence < self.config.min_confidence {
            return AutomationOutput::idle(Some(snapshot), None);
        }

        let entry_filter = self.selector.evaluate(&snapshot);
        if self.machine.state.phase == StrategyPhase::Flat && !entry_filter.allowed {
            return AutomationOutput::idle(Some(snapshot), Some(entry_filter));
        }

        if let Some(last_emit_ns) = self.last_emit_ns
            && now_ns.saturating_sub(last_emit_ns) < self.config.min_emit_interval_ns
        {
            return AutomationOutput::idle(Some(snapshot), Some(entry_filter));
        }

        self.emission_sequence = self.emission_sequence.saturating_add(1);
        let signal = Signal {
            schema_version: "signal.v1".into(),
            signal_id: format!(
                "live:{}:{}:{}:{}",
                self.machine.config.strategy_id,
                self.machine.config.asset,
                now_ns,
                self.emission_sequence
            ),
            alpha_id: self.config.alpha_id.clone(),
            asset: self.machine.config.asset.clone(),
            venue: self.machine.config.venue,
            score: snapshot.score,
            confidence: snapshot.confidence,
            horizon_ms: self.config.signal_horizon_ms,
            created_at_ns: now_ns,
            expires_at_ns: now_ns
                .saturating_add(self.config.signal_ttl_ms.saturating_mul(1_000_000)),
            model_version: None,
            feature_set: Some("live-microstructure-v1".into()),
            metadata: json!({
                "vwap": snapshot.vwap,
                "vwap_deviation_bps": snapshot.vwap_deviation_bps,
                "trade_imbalance": snapshot.trade_imbalance,
                "spread_bps": snapshot.spread_bps,
                "book_imbalance": snapshot.book_imbalance,
                "momentum_bps": snapshot.momentum_bps,
                "realized_volatility_bps": snapshot.realized_volatility_bps,
                "warmup_ratio": snapshot.warmup_ratio,
                "entry_filter_allowed": entry_filter.allowed,
                "volume_ratio": entry_filter.volume_ratio,
                "entry_filter_reasons": entry_filter.reasons,
            }),
        };
        self.last_emit_ns = Some(now_ns);
        let decision = self.machine.on_signal(&signal, now_ns);
        AutomationOutput {
            factors: Some(snapshot),
            entry_filter: Some(entry_filter),
            signal: Some(signal),
            decision,
        }
    }
}

fn event_scope(event: &MarketEvent) -> (Venue, &str, u64) {
    match event {
        MarketEvent::Trade(value) => (value.venue, &value.asset, value.ts_recv_ns),
        MarketEvent::BestBidAsk(value) => (value.venue, &value.asset, value.ts_recv_ns),
        MarketEvent::L2Book(value) => (value.venue, &value.asset, value.ts_recv_ns),
        MarketEvent::Candle(value) => (value.venue, &value.asset, value.ts_recv_ns),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_marketdata::{AggressorSide, BestBidAsk, BookLevel, Candle, L2Book, TradeTick};
    use pg_types::{ExposureEffect, Venue};
    use rust_decimal::Decimal;

    fn strategy() -> StrategyConfig {
        StrategyConfig {
            strategy_id: "micro-hype".into(),
            asset: "HYPE".into(),
            venue: Venue::Hyperliquid,
            order_quantity: Decimal::ONE,
            entry_score: 0.25,
            exit_score: 0.05,
        }
    }

    fn automation() -> StrategyAutomationConfig {
        StrategyAutomationConfig {
            factors: FactorConfig {
                trade_window: 4,
                candle_window: 2,
                book_levels: 2,
                min_trade_samples: 2,
                min_candle_samples: 2,
                max_spread_bps: 200.0,
                volatility_soft_cap_bps: 1_000.0,
                vwap_scale_bps: 100.0,
                momentum_scale_bps: 100.0,
                vwap_weight: 0.20,
                trade_imbalance_weight: 0.35,
                book_imbalance_weight: 0.30,
                momentum_weight: 0.15,
            },
            candle_interval_ns: 5_000_000_000,
            signal_horizon_ms: 5_000,
            signal_ttl_ms: 2_000,
            min_confidence: 0.30,
            min_emit_interval_ns: 0,
            alpha_id: "test.live".into(),
            entry_filter: EntryFilterConfig::default(),
        }
    }

    #[test]
    fn declares_all_base_subscriptions() {
        let automated = AutomatedStrategy::new(strategy(), automation()).unwrap();
        let feeds = automated.subscriptions();
        assert_eq!(feeds.len(), 4);
        assert!(
            feeds
                .iter()
                .any(|feed| matches!(feed.kind, FeedKind::Trades))
        );
        assert!(
            feeds
                .iter()
                .any(|feed| matches!(feed.kind, FeedKind::BestBidAsk))
        );
        assert!(
            feeds
                .iter()
                .any(|feed| matches!(feed.kind, FeedKind::L2Book))
        );
        assert!(
            feeds
                .iter()
                .any(|feed| matches!(feed.kind, FeedKind::Candle { .. }))
        );
    }

    #[test]
    fn market_events_can_drive_an_entry_intent() {
        let mut automated = AutomatedStrategy::new(strategy(), automation()).unwrap();
        let events = vec![
            MarketEvent::BestBidAsk(BestBidAsk {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 1,
                ts_recv_ns: 1,
                bid_price: Decimal::from(100),
                bid_quantity: Decimal::from(20),
                ask_price: Decimal::from(101),
                ask_quantity: Decimal::from(5),
                sequence: None,
            }),
            MarketEvent::L2Book(L2Book {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 2,
                ts_recv_ns: 2,
                bids: vec![BookLevel {
                    price: Decimal::from(100),
                    quantity: Decimal::from(30),
                    order_count: None,
                }],
                asks: vec![BookLevel {
                    price: Decimal::from(101),
                    quantity: Decimal::from(5),
                    order_count: None,
                }],
                sequence: None,
                is_snapshot: true,
            }),
            MarketEvent::Trade(TradeTick {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 3,
                ts_recv_ns: 3,
                price: Decimal::from(101),
                quantity: Decimal::ONE,
                aggressor: AggressorSide::Buy,
                sequence: None,
            }),
            MarketEvent::Trade(TradeTick {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 4,
                ts_recv_ns: 4,
                price: Decimal::from(102),
                quantity: Decimal::ONE,
                aggressor: AggressorSide::Buy,
                sequence: None,
            }),
            MarketEvent::Candle(Candle {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                interval_ns: 5_000_000_000,
                start_ns: 5,
                end_ns: 6,
                ts_recv_ns: 5,
                open: Decimal::from(101),
                high: Decimal::from(101),
                low: Decimal::from(101),
                close: Decimal::from(101),
                volume: Decimal::ONE,
                trades: 1,
            }),
            MarketEvent::Candle(Candle {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                interval_ns: 5_000_000_000,
                start_ns: 6,
                end_ns: 7,
                ts_recv_ns: 6,
                open: Decimal::from(103),
                high: Decimal::from(103),
                low: Decimal::from(103),
                close: Decimal::from(103),
                volume: Decimal::ONE,
                trades: 1,
            }),
        ];

        let mut submitted = None;
        for event in &events {
            let output = automated.on_market_event(event);
            if let StrategyDecision::Submit(intent) = output.decision {
                submitted = Some(intent);
                break;
            }
        }
        let intent = submitted.expect("expected automated entry");
        assert_eq!(intent.effect, ExposureEffect::Increase);
    }

    #[test]
    fn entry_filter_blocks_flat_entries_until_volume_confirms() {
        let mut config = automation();
        config.entry_filter = EntryFilterConfig {
            enabled: true,
            volume_window: 3,
            min_volume_samples: 3,
            min_momentum_bps: Some(50.0),
            min_volume_ratio: Some(1.5),
            ..EntryFilterConfig::default()
        };
        let mut automated = AutomatedStrategy::new(strategy(), config).unwrap();

        for event in [
            MarketEvent::BestBidAsk(BestBidAsk {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 1,
                ts_recv_ns: 1,
                bid_price: Decimal::from(100),
                bid_quantity: Decimal::from(20),
                ask_price: Decimal::from(101),
                ask_quantity: Decimal::from(5),
                sequence: None,
            }),
            MarketEvent::L2Book(L2Book {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 2,
                ts_recv_ns: 2,
                bids: vec![BookLevel {
                    price: Decimal::from(100),
                    quantity: Decimal::from(30),
                    order_count: None,
                }],
                asks: vec![BookLevel {
                    price: Decimal::from(101),
                    quantity: Decimal::from(5),
                    order_count: None,
                }],
                sequence: None,
                is_snapshot: true,
            }),
            MarketEvent::Trade(TradeTick {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 3,
                ts_recv_ns: 3,
                price: Decimal::from(101),
                quantity: Decimal::ONE,
                aggressor: AggressorSide::Buy,
                sequence: None,
            }),
            MarketEvent::Trade(TradeTick {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                ts_event_ns: 4,
                ts_recv_ns: 4,
                price: Decimal::from(102),
                quantity: Decimal::ONE,
                aggressor: AggressorSide::Buy,
                sequence: None,
            }),
        ] {
            automated.on_market_event(&event);
        }

        for (ts, close, volume) in [(5, 100, 10), (6, 101, 10)] {
            let output = automated.on_market_event(&MarketEvent::Candle(Candle {
                venue: Venue::Hyperliquid,
                asset: "HYPE".into(),
                interval_ns: 5_000_000_000,
                start_ns: ts,
                end_ns: ts + 1,
                ts_recv_ns: ts,
                open: Decimal::from(close),
                high: Decimal::from(close),
                low: Decimal::from(close),
                close: Decimal::from(close),
                volume: Decimal::from(volume),
                trades: 1,
            }));
            assert!(!matches!(output.decision, StrategyDecision::Submit(_)));
        }

        let output = automated.on_market_event(&MarketEvent::Candle(Candle {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            interval_ns: 5_000_000_000,
            start_ns: 7,
            end_ns: 8,
            ts_recv_ns: 7,
            open: Decimal::from(103),
            high: Decimal::from(103),
            low: Decimal::from(103),
            close: Decimal::from(103),
            volume: Decimal::from(30),
            trades: 1,
        }));
        assert!(
            output
                .entry_filter
                .as_ref()
                .is_some_and(|filter| filter.allowed)
        );
        assert!(matches!(output.decision, StrategyDecision::Submit(_)));
    }
}
