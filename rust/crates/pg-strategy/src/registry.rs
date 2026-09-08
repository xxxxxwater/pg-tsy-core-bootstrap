use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pg_marketdata::{FeedSpec, MarketEvent};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedPolicyOutput {
    pub strategy_id: String,
    pub instrument: AssetKey,
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

    pub fn subscriptions(&self) -> Vec<FeedSpec> {
        self.strategies
            .values()
            .flat_map(AutomatedStrategy::subscriptions)
            .collect()
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

    pub fn get_mut(&mut self, strategy_id: &str) -> Option<&mut AutomatedStrategy> {
        self.strategies.get_mut(strategy_id)
    }
}

fn event_matches_strategy(event: &MarketEvent, strategy: &AutomatedStrategy) -> bool {
    match event {
        MarketEvent::Trade(value) => {
            value.venue == strategy.machine.config.venue
                && value.asset == strategy.machine.config.asset
        }
        MarketEvent::BestBidAsk(value) => {
            value.venue == strategy.machine.config.venue
                && value.asset == strategy.machine.config.asset
        }
        MarketEvent::L2Book(value) => {
            value.venue == strategy.machine.config.venue
                && value.asset == strategy.machine.config.asset
        }
        MarketEvent::Candle(value) => {
            value.venue == strategy.machine.config.venue
                && value.asset == strategy.machine.config.asset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::graph::EntryPolicyDecision;
    use pg_types::Venue;
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

                [automation.entry_filter]
                enabled = true
                min_momentum_bps = 25.0
                min_volume_ratio = 1.25

                [[policy.entries]]
                id = "momentum"
                side = "Buy"
                mode = "all"

                [[policy.entries.predicates]]
                feature = "momentum_bps"
                op = "gte"
                value = 25.0
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

        let mut features = FeatureFrame::default();
        features.insert("momentum_bps", 30.0);
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
}
