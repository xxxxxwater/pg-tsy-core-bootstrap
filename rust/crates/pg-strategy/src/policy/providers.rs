use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pg_marketdata::{FeedKind, FeedSpec, MarketEvent};
use pg_types::AssetKey;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::factors::{FactorConfig, FactorSnapshot, RollingFactorEngine};

use super::{FeatureFrame, PositionView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedRequirement {
    Trades,
    BestBidAsk,
    L2Book,
    Candle,
}

impl FeedRequirement {
    fn feed_kind(self, candle_interval_ns: u64) -> FeedKind {
        match self {
            Self::Trades => FeedKind::Trades,
            Self::BestBidAsk => FeedKind::BestBidAsk,
            Self::L2Book => FeedKind::L2Book,
            Self::Candle => FeedKind::Candle {
                interval_ns: candle_interval_ns,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeatureProviderSpec {
    pub feature: String,
    pub feeds: BTreeSet<FeedRequirement>,
}

impl FeatureProviderSpec {
    fn new(feature: impl Into<String>, feeds: impl IntoIterator<Item = FeedRequirement>) -> Self {
        Self {
            feature: feature.into(),
            feeds: feeds.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FeatureProviderRegistry {
    providers: BTreeMap<String, FeatureProviderSpec>,
}

impl FeatureProviderRegistry {
    pub fn standard() -> Self {
        let mut registry = Self::default();
        for feature in [
            "last_price",
            "vwap",
            "vwap_deviation_bps",
            "trade_imbalance",
        ] {
            registry.register(FeatureProviderSpec::new(feature, [FeedRequirement::Trades]));
        }
        registry.register(FeatureProviderSpec::new(
            "spread_bps",
            [FeedRequirement::BestBidAsk],
        ));
        registry.register(FeatureProviderSpec::new(
            "book_imbalance",
            [FeedRequirement::L2Book],
        ));
        for feature in ["momentum_bps", "realized_volatility_bps", "volume_ratio"] {
            registry.register(FeatureProviderSpec::new(feature, [FeedRequirement::Candle]));
        }
        for feature in ["warmup_ratio", "score", "confidence"] {
            registry.register(FeatureProviderSpec::new(
                feature,
                [
                    FeedRequirement::Trades,
                    FeedRequirement::BestBidAsk,
                    FeedRequirement::L2Book,
                    FeedRequirement::Candle,
                ],
            ));
        }
        for feature in [
            "net_quantity",
            "average_entry_price",
            "filled_entries",
            "unrealized_return",
            "peak_return",
            "position.net_quantity",
            "position.average_entry_price",
            "position.filled_entries",
            "position.unrealized_return",
            "position.peak_return",
        ] {
            registry.register(FeatureProviderSpec::new(feature, []));
        }
        registry
    }

    pub fn register(&mut self, provider: FeatureProviderSpec) {
        self.providers.insert(provider.feature.clone(), provider);
    }

    pub fn contains(&self, feature: &str) -> bool {
        self.providers.contains_key(feature)
    }

    pub fn plan(&self, required_features: &BTreeSet<String>) -> Result<FeaturePlan, String> {
        let mut feeds = BTreeSet::new();
        let mut unresolved = Vec::new();
        for feature in required_features {
            match self.providers.get(feature) {
                Some(provider) => feeds.extend(provider.feeds.iter().copied()),
                None => unresolved.push(feature.clone()),
            }
        }
        if !unresolved.is_empty() {
            return Err(format!(
                "no live feature provider registered for: {}",
                unresolved.join(", ")
            ));
        }
        Ok(FeaturePlan {
            required_features: required_features.clone(),
            feeds,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeaturePlan {
    required_features: BTreeSet<String>,
    feeds: BTreeSet<FeedRequirement>,
}

impl FeaturePlan {
    pub fn required_features(&self) -> &BTreeSet<String> {
        &self.required_features
    }

    pub fn feed_requirements(&self) -> &BTreeSet<FeedRequirement> {
        &self.feeds
    }

    pub fn subscriptions(&self, instrument: &AssetKey, candle_interval_ns: u64) -> Vec<FeedSpec> {
        self.feeds
            .iter()
            .copied()
            .map(|requirement| FeedSpec {
                venue: instrument.venue,
                asset: instrument.asset.clone(),
                kind: requirement.feed_kind(candle_interval_ns),
            })
            .collect()
    }

    fn requires_event(&self, event: &MarketEvent) -> bool {
        let requirement = match event {
            MarketEvent::Trade(_) => FeedRequirement::Trades,
            MarketEvent::BestBidAsk(_) => FeedRequirement::BestBidAsk,
            MarketEvent::L2Book(_) => FeedRequirement::L2Book,
            MarketEvent::Candle(_) => FeedRequirement::Candle,
        };
        self.feeds.contains(&requirement)
    }

    fn requires(&self, feature: &str) -> bool {
        self.required_features.contains(feature)
    }
}

pub struct LiveFeatureEngine {
    plan: FeaturePlan,
    factors: RollingFactorEngine,
    candle_interval_ns: u64,
    volumes: VecDeque<f64>,
    volume_window: usize,
    min_volume_samples: usize,
}

impl LiveFeatureEngine {
    pub fn new(
        plan: FeaturePlan,
        factor_config: FactorConfig,
        candle_interval_ns: u64,
        volume_window: usize,
        min_volume_samples: usize,
    ) -> Result<Self, String> {
        if candle_interval_ns == 0 {
            return Err("feature runtime candle_interval_ns must be positive".into());
        }
        if volume_window < 2 {
            return Err("feature runtime volume_window must be >= 2".into());
        }
        if min_volume_samples < 2 || min_volume_samples > volume_window {
            return Err("feature runtime min_volume_samples must be in [2, volume_window]".into());
        }
        let factors = RollingFactorEngine::new(factor_config).map_err(str::to_owned)?;
        Ok(Self {
            plan,
            factors,
            candle_interval_ns,
            volumes: VecDeque::new(),
            volume_window,
            min_volume_samples,
        })
    }

    pub fn subscriptions(&self, instrument: &AssetKey) -> Vec<FeedSpec> {
        self.plan.subscriptions(instrument, self.candle_interval_ns)
    }

    pub fn on_event(
        &mut self,
        event: &MarketEvent,
        position: &PositionView,
    ) -> Option<FeatureFrame> {
        if !self.plan.requires_event(event) {
            return None;
        }
        if let MarketEvent::Candle(candle) = event {
            if candle.interval_ns != self.candle_interval_ns {
                return None;
            }
            if self.plan.requires("volume_ratio") {
                let volume = candle.volume.to_f64()?;
                if volume.is_finite() && volume >= 0.0 {
                    self.volumes.push_back(volume);
                    while self.volumes.len() > self.volume_window {
                        self.volumes.pop_front();
                    }
                }
            }
        }
        self.factors.on_event(event)?;
        Some(self.snapshot(position))
    }

    pub fn snapshot(&self, position: &PositionView) -> FeatureFrame {
        let factors = self.factors.snapshot();
        let mut frame = FeatureFrame::default();
        self.insert_factor_snapshot(&mut frame, &factors);
        if self.plan.requires("volume_ratio")
            && let Some(value) = self.volume_ratio()
        {
            frame.insert("volume_ratio", value);
        }
        self.insert_position_features(&mut frame, position);
        frame
    }

    fn insert_factor_snapshot(&self, frame: &mut FeatureFrame, snapshot: &FactorSnapshot) {
        macro_rules! insert_optional {
            ($name:literal, $value:expr) => {
                if self.plan.requires($name) {
                    if let Some(value) = $value {
                        frame.insert($name, value);
                    }
                }
            };
        }
        insert_optional!("last_price", snapshot.last_price);
        insert_optional!("vwap", snapshot.vwap);
        insert_optional!("vwap_deviation_bps", snapshot.vwap_deviation_bps);
        insert_optional!("trade_imbalance", snapshot.trade_imbalance);
        insert_optional!("spread_bps", snapshot.spread_bps);
        insert_optional!("book_imbalance", snapshot.book_imbalance);
        insert_optional!("momentum_bps", snapshot.momentum_bps);
        insert_optional!("realized_volatility_bps", snapshot.realized_volatility_bps);
        if self.plan.requires("warmup_ratio") {
            frame.insert("warmup_ratio", snapshot.warmup_ratio);
        }
        if self.plan.requires("score") {
            frame.insert("score", snapshot.score);
        }
        if self.plan.requires("confidence") {
            frame.insert("confidence", snapshot.confidence);
        }
    }

    fn insert_position_features(&self, frame: &mut FeatureFrame, position: &PositionView) {
        self.insert_position_scalar(frame, "net_quantity", position.net_quantity.to_f64());
        self.insert_position_scalar(
            frame,
            "average_entry_price",
            position
                .average_entry_price
                .and_then(|value| value.to_f64()),
        );
        self.insert_position_scalar(
            frame,
            "filled_entries",
            Some(position.filled_entries as f64),
        );
        self.insert_position_scalar(frame, "unrealized_return", position.unrealized_return);
        self.insert_position_scalar(frame, "peak_return", position.peak_return);
    }

    fn insert_position_scalar(&self, frame: &mut FeatureFrame, name: &str, value: Option<f64>) {
        let Some(value) = value else {
            return;
        };
        if self.plan.requires(name) {
            frame.insert(name, value);
        }
        let alias = format!("position.{name}");
        if self.plan.requires(&alias) {
            frame.insert(alias, value);
        }
    }

    fn volume_ratio(&self) -> Option<f64> {
        if self.volumes.len() < self.min_volume_samples {
            return None;
        }
        let latest = *self.volumes.back()?;
        let baseline_count = self.volumes.len().saturating_sub(1);
        if baseline_count == 0 {
            return None;
        }
        let baseline =
            self.volumes.iter().take(baseline_count).sum::<f64>() / baseline_count as f64;
        (baseline > 0.0).then_some(latest / baseline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_marketdata::{BestBidAsk, Candle};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    #[test]
    fn feature_plan_derives_minimal_subscriptions() {
        let registry = FeatureProviderRegistry::standard();
        let required = ["momentum_bps", "volume_ratio", "spread_bps"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let plan = registry.plan(&required).unwrap();
        assert_eq!(plan.feed_requirements().len(), 2);
        assert!(plan.feed_requirements().contains(&FeedRequirement::Candle));
        assert!(
            plan.feed_requirements()
                .contains(&FeedRequirement::BestBidAsk)
        );
    }

    #[test]
    fn unregistered_live_feature_is_rejected() {
        let registry = FeatureProviderRegistry::standard();
        let required = ["custom_unregistered_factor"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(registry.plan(&required).is_err());
    }

    #[test]
    fn live_events_produce_the_same_named_feature_frame_used_by_policy_replay() {
        let registry = FeatureProviderRegistry::standard();
        let required = ["momentum_bps", "volume_ratio", "spread_bps"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let plan = registry.plan(&required).unwrap();
        let config = FactorConfig {
            candle_window: 3,
            min_candle_samples: 2,
            ..FactorConfig::default()
        };
        let mut engine = LiveFeatureEngine::new(plan, config, 300, 3, 3).unwrap();
        let position = PositionView::default();
        let bbo = MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: 1,
            ts_recv_ns: 1,
            bid_price: Decimal::from(100),
            bid_quantity: Decimal::ONE,
            ask_price: Decimal::new(1001, 1),
            ask_quantity: Decimal::ONE,
            sequence: None,
        });
        engine.on_event(&bbo, &position).unwrap();
        for (ts, close, volume) in [(2, 100, 10), (3, 101, 10), (4, 103, 30)] {
            engine
                .on_event(
                    &MarketEvent::Candle(Candle {
                        venue: Venue::Hyperliquid,
                        asset: "HYPE".into(),
                        interval_ns: 300,
                        start_ns: ts,
                        end_ns: ts + 1,
                        ts_recv_ns: ts,
                        open: Decimal::from(close),
                        high: Decimal::from(close),
                        low: Decimal::from(close),
                        close: Decimal::from(close),
                        volume: Decimal::from(volume),
                        trades: 1,
                    }),
                    &position,
                )
                .unwrap();
        }
        let frame = engine.snapshot(&position);
        assert!(frame.get("momentum_bps").unwrap() >= 299.0);
        assert_eq!(frame.get("volume_ratio"), Some(3.0));
        assert!(frame.get("spread_bps").unwrap() < 20.0);
    }
}
