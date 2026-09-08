use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pg_marketdata::{FeedKind, FeedSpec, MarketEvent};
use pg_types::AssetKey;
use thiserror::Error;

use crate::StrategyPhase;
use crate::automation::{AutomatedStrategy, AutomationOutput};
use crate::definition::{PolicyInstance, StrategyDefinition, StrategyDefinitionError};
use crate::policy::graph::{EntryPolicyDecision, ExitPolicyDecision};
use crate::policy::{FeatureFrame, PositionView, StrategyContext};

#[derive(Debug, Error)]
pub enum StrategyRegistryError {
    #[error("strategy directory IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Definition(#[from] StrategyDefinitionError),
    #[error("duplicate strategy id: {0}")]
    DuplicateStrategy(String),
    #[error("cannot reload {0}: strategy has active or unresolved state")]
    ActiveState(String),
}

pub struct RoutedAutomationOutput {
    pub strategy_id: String,
    pub output: AutomationOutput,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedPolicyOutput {
    pub strategy_id: String,
    pub instrument: AssetKey,
    pub entry: EntryPolicyDecision,
    pub exit: ExitPolicyDecision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedLivePolicyOutput {
    pub strategy_id: String,
    pub instrument: AssetKey,
    pub features: FeatureFrame,
    pub entry: EntryPolicyDecision,
    pub exit: ExitPolicyDecision,
}

pub struct StrategyRegistry {
    strategies: BTreeMap<String, AutomatedStrategy>,
    policies: BTreeMap<String, PolicyInstance>,
    sources: BTreeMap<PathBuf, Vec<String>>,
}

impl StrategyRegistry {
    pub fn empty() -> Self {
        Self {
            strategies: BTreeMap::new(),
            policies: BTreeMap::new(),
            sources: BTreeMap::new(),
        }
    }

    pub fn load_dir(path: impl AsRef<Path>) -> Result<Self, StrategyRegistryError> {
        let path = path.as_ref();
        let mut entries = fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("toml"))
            .collect::<Vec<_>>();
        entries.sort();

        let mut registry = Self::empty();
        for entry in entries {
            registry.load_file(&entry)?;
        }
        Ok(registry)
    }

    pub fn load_file(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<String>, StrategyRegistryError> {
        let path = path.as_ref().to_path_buf();
        let definition = StrategyDefinition::from_toml_str(&fs::read_to_string(&path)?)?;
        let instances = definition.build_instances()?;
        let policy_instances = definition.build_policy_instances()?;
        let mut ids = Vec::with_capacity(instances.len());
        for strategy in instances {
            let id = strategy.machine.config.strategy_id.clone();
            if self.strategies.contains_key(&id) {
                return Err(StrategyRegistryError::DuplicateStrategy(id));
            }
            ids.push(id.clone());
            self.strategies.insert(id, strategy);
        }
        for policy in policy_instances {
            self.policies.insert(policy.strategy_id.clone(), policy);
        }
        self.sources.insert(path, ids.clone());
        Ok(ids)
    }

    pub fn reload_file(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<String>, StrategyRegistryError> {
        let path = path.as_ref().to_path_buf();
        let old_ids = self.sources.get(&path).cloned().unwrap_or_default();
        for id in &old_ids {
            if let Some(strategy) = self.strategies.get(id)
                && (strategy.machine.state.net_quantity != rust_decimal::Decimal::ZERO
                    || strategy.machine.state.active_intent_id.is_some()
                    || !matches!(
                        strategy.machine.state.phase,
                        StrategyPhase::Flat | StrategyPhase::Halted
                    ))
            {
                return Err(StrategyRegistryError::ActiveState(id.clone()));
            }
        }

        let definition = StrategyDefinition::from_toml_str(&fs::read_to_string(&path)?)?;
        let instances = definition.build_instances()?;
        let policy_instances = definition.build_policy_instances()?;
        let new_ids = instances
            .iter()
            .map(|strategy| strategy.machine.config.strategy_id.clone())
            .collect::<Vec<_>>();

        for id in &new_ids {
            if self.strategies.contains_key(id) && !old_ids.contains(id) {
                return Err(StrategyRegistryError::DuplicateStrategy(id.clone()));
            }
        }

        for id in &old_ids {
            self.strategies.remove(id);
            self.policies.remove(id);
        }
        for strategy in instances {
            self.strategies
                .insert(strategy.machine.config.strategy_id.clone(), strategy);
        }
        for policy in policy_instances {
            self.policies.insert(policy.strategy_id.clone(), policy);
        }
        self.sources.insert(path, new_ids.clone());
        Ok(new_ids)
    }

    pub fn strategy_ids(&self) -> Vec<String> {
        self.strategies.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.strategies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strategies.is_empty()
    }

    pub fn policy_count(&self) -> usize {
        self.policies.len()
    }

    pub fn policy_subscriptions(&self) -> Vec<FeedSpec> {
        let mut feeds = Vec::new();
        for spec in self
            .policies
            .values()
            .flat_map(PolicyInstance::subscriptions)
        {
            push_unique_feed(&mut feeds, spec);
        }
        feeds
    }

    pub fn subscriptions(&self) -> Vec<FeedSpec> {
        let mut feeds = Vec::new();
        for spec in self
            .strategies
            .values()
            .flat_map(AutomatedStrategy::subscriptions)
            .chain(self.policy_subscriptions())
        {
            push_unique_feed(&mut feeds, spec);
        }
        feeds
    }

    pub fn route_event(&mut self, event: &MarketEvent) -> Vec<RoutedAutomationOutput> {
        self.strategies
            .iter_mut()
            .filter(|(_, strategy)| event_matches_strategy(event, strategy))
            .map(|(id, strategy)| RoutedAutomationOutput {
                strategy_id: id.clone(),
                output: strategy.on_market_event(event),
            })
            .collect()
    }

    pub fn route_policy_frame(
        &self,
        instrument: &AssetKey,
        features: &FeatureFrame,
        position: &PositionView,
        now_ns: u64,
    ) -> Vec<RoutedPolicyOutput> {
        self.policies
            .values()
            .filter(|policy| &policy.instrument == instrument)
            .map(|policy| {
                let context = StrategyContext {
                    instrument,
                    features,
                    position,
                    now_ns,
                };
                RoutedPolicyOutput {
                    strategy_id: policy.strategy_id.clone(),
                    instrument: instrument.clone(),
                    entry: policy.engine.evaluate_entry(&context),
                    exit: policy.engine.evaluate_exit(&context),
                }
            })
            .collect()
    }

    pub fn route_live_policy_event(
        &mut self,
        event: &MarketEvent,
        position: &PositionView,
    ) -> Vec<RoutedLivePolicyOutput> {
        let instrument = event_asset_key(event);
        self.policies
            .values_mut()
            .filter(|policy| policy.instrument == instrument)
            .filter_map(|policy| {
                let features = policy.features.on_event(event, position)?;
                let context = StrategyContext {
                    instrument: &policy.instrument,
                    features: &features,
                    position,
                    now_ns: event.ts_recv_ns(),
                };
                Some(RoutedLivePolicyOutput {
                    strategy_id: policy.strategy_id.clone(),
                    instrument: policy.instrument.clone(),
                    entry: policy.engine.evaluate_entry(&context),
                    exit: policy.engine.evaluate_exit(&context),
                    features,
                })
            })
            .collect()
    }

    pub fn get_mut(&mut self, strategy_id: &str) -> Option<&mut AutomatedStrategy> {
        self.strategies.get_mut(strategy_id)
    }
}

fn event_matches_strategy(event: &MarketEvent, strategy: &AutomatedStrategy) -> bool {
    let key = event_asset_key(event);
    key.venue == strategy.machine.config.venue && key.asset == strategy.machine.config.asset
}

fn event_asset_key(event: &MarketEvent) -> AssetKey {
    match event {
        MarketEvent::Trade(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::BestBidAsk(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::L2Book(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::Candle(value) => AssetKey::new(value.venue, value.asset.clone()),
    }
}

fn push_unique_feed(feeds: &mut Vec<FeedSpec>, candidate: FeedSpec) {
    if !feeds.iter().any(|existing| same_feed(existing, &candidate)) {
        feeds.push(candidate);
    }
}

fn same_feed(left: &FeedSpec, right: &FeedSpec) -> bool {
    if left.venue != right.venue || left.asset != right.asset {
        return false;
    }
    match (&left.kind, &right.kind) {
        (FeedKind::Trades, FeedKind::Trades)
        | (FeedKind::BestBidAsk, FeedKind::BestBidAsk)
        | (FeedKind::L2Book, FeedKind::L2Book) => true,
        (
            FeedKind::Candle {
                interval_ns: left_interval,
            },
            FeedKind::Candle {
                interval_ns: right_interval,
            },
        ) => left_interval == right_interval,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::graph::EntryPolicyDecision;
    use pg_marketdata::{BestBidAsk, Candle};
    use pg_types::Venue;
    use rust_decimal::Decimal;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pg-strategy-registry-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn definition(entry_score: f64) -> String {
        format!(
            r#"
                [universe]
                venue = "HYPERLIQUID"
                assets = ["HYPE", "SOL"]

                [strategy]
                id = "mv"
                order_quantity = "1"
                entry_score = {entry_score}
                exit_score = 0.05

                [automation]
                candle_interval_ns = 300

                [automation.factors]
                candle_window = 3
                min_candle_samples = 2

                [automation.entry_filter]
                enabled = true
                volume_window = 3
                min_volume_samples = 3
                min_momentum_bps = 25.0
                min_volume_ratio = 1.25

                [[policy.filters]]
                id = "spread"
                mode = "all"

                [[policy.filters.predicates]]
                feature = "spread_bps"
                op = "lte"
                value = 20.0

                [[policy.entries]]
                id = "momentum-volume"
                side = "Buy"
                mode = "all"

                [[policy.entries.predicates]]
                feature = "momentum_bps"
                op = "gte"
                value = 25.0

                [[policy.entries.predicates]]
                feature = "volume_ratio"
                op = "gte"
                value = 1.25
            "#
        )
    }

    #[test]
    fn directory_loads_and_flat_strategies_can_reload() {
        let dir = temp_dir();
        let file = dir.join("mv.toml");
        fs::write(&file, definition(0.30)).unwrap();
        let mut registry = StrategyRegistry::load_dir(&dir).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.policy_count(), 2);
        assert_eq!(registry.subscriptions().len(), 8);
        assert_eq!(registry.policy_subscriptions().len(), 4);

        let mut features = FeatureFrame::default();
        features.insert("momentum_bps", 30.0);
        features.insert("volume_ratio", 1.5);
        features.insert("spread_bps", 5.0);
        let routed = registry.route_policy_frame(
            &AssetKey::new(Venue::Hyperliquid, "HYPE"),
            &features,
            &PositionView::default(),
            1,
        );
        assert_eq!(routed.len(), 1);
        assert!(matches!(
            routed[0].entry,
            EntryPolicyDecision::Matched { .. }
        ));

        fs::write(&file, definition(0.40)).unwrap();
        let ids = registry.reload_file(&file).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            registry
                .get_mut("mv:HYPE")
                .unwrap()
                .machine
                .config
                .entry_score,
            0.40
        );

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn live_market_events_build_normalized_features_before_policy_evaluation() {
        let dir = temp_dir();
        let file = dir.join("mv.toml");
        fs::write(&file, definition(0.30)).unwrap();
        let mut registry = StrategyRegistry::load_dir(&dir).unwrap();
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
        registry.route_live_policy_event(&bbo, &position);

        let mut last = Vec::new();
        for (ts, close, volume) in [(2, 100, 10), (3, 101, 10), (4, 103, 30)] {
            last = registry.route_live_policy_event(
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
            );
        }

        assert_eq!(last.len(), 1);
        assert!(last[0].features.get("momentum_bps").unwrap() >= 299.0);
        assert_eq!(last[0].features.get("volume_ratio"), Some(3.0));
        assert!(matches!(last[0].entry, EntryPolicyDecision::Matched { .. }));
        fs::remove_dir_all(dir).ok();
    }
}
