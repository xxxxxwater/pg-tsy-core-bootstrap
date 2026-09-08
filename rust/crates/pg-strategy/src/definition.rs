use std::collections::BTreeSet;
use std::str::FromStr;

use pg_types::{AssetKey, Venue};
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

use crate::StrategyConfig;
use crate::automation::{AutomatedStrategy, StrategyAutomationConfig};
use crate::factors::FactorConfig;
use crate::policy::graph::{PolicyDefinition, PolicyEngine};
use crate::selector::EntryFilterConfig;

#[derive(Debug, Error)]
pub enum StrategyDefinitionError {
    #[error("invalid TOML strategy definition: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid strategy definition: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyDefinition {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub universe: UniverseDefinition,
    pub strategy: StrategyTemplateDefinition,
    #[serde(default)]
    pub automation: AutomationOverrides,
    #[serde(default)]
    pub policy: PolicyDefinition,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UniverseDefinition {
    /// Legacy/single-venue form. Kept for compatibility with existing definitions.
    #[serde(default)]
    pub venue: Option<Venue>,
    #[serde(default)]
    pub assets: Vec<String>,
    /// Standard multi-venue form. Each strategy instance still owns one AssetKey.
    #[serde(default)]
    pub instruments: Vec<InstrumentDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentDefinition {
    pub venue: Venue,
    pub asset: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyTemplateDefinition {
    pub id: String,
    pub order_quantity: String,
    pub entry_score: f64,
    pub exit_score: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutomationOverrides {
    pub candle_interval_ns: Option<u64>,
    pub signal_horizon_ms: Option<u64>,
    pub signal_ttl_ms: Option<u64>,
    pub min_confidence: Option<f64>,
    pub min_emit_interval_ns: Option<u64>,
    pub alpha_id: Option<String>,
    #[serde(default)]
    pub factors: FactorOverrides,
    #[serde(default)]
    pub entry_filter: EntryFilterConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FactorOverrides {
    pub trade_window: Option<usize>,
    pub candle_window: Option<usize>,
    pub book_levels: Option<usize>,
    pub min_trade_samples: Option<usize>,
    pub min_candle_samples: Option<usize>,
    pub max_spread_bps: Option<f64>,
    pub volatility_soft_cap_bps: Option<f64>,
    pub vwap_scale_bps: Option<f64>,
    pub momentum_scale_bps: Option<f64>,
    pub vwap_weight: Option<f64>,
    pub trade_imbalance_weight: Option<f64>,
    pub book_imbalance_weight: Option<f64>,
    pub momentum_weight: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct PolicyInstance {
    pub strategy_id: String,
    pub instrument: AssetKey,
    pub engine: PolicyEngine,
}

impl UniverseDefinition {
    pub fn resolved_instruments(&self) -> Result<Vec<AssetKey>, StrategyDefinitionError> {
        let using_legacy = self.venue.is_some() || !self.assets.is_empty();
        let using_standard = !self.instruments.is_empty();
        if using_legacy && using_standard {
            return Err(StrategyDefinitionError::Invalid(
                "universe must use either venue+assets or instruments, not both".into(),
            ));
        }

        let instruments = if using_standard {
            self.instruments
                .iter()
                .map(|instrument| AssetKey::new(instrument.venue, instrument.asset.trim()))
                .collect::<Vec<_>>()
        } else {
            let venue = self.venue.ok_or_else(|| {
                StrategyDefinitionError::Invalid(
                    "legacy universe requires venue when assets are used".into(),
                )
            })?;
            if self.assets.is_empty() {
                return Err(StrategyDefinitionError::Invalid(
                    "universe.assets or universe.instruments must not be empty".into(),
                ));
            }
            self.assets
                .iter()
                .map(|asset| AssetKey::new(venue, asset.trim()))
                .collect::<Vec<_>>()
        };

        if instruments.is_empty() {
            return Err(StrategyDefinitionError::Invalid(
                "universe must contain at least one instrument".into(),
            ));
        }

        let mut unique = BTreeSet::new();
        for instrument in &instruments {
            if instrument.asset.trim().is_empty() {
                return Err(StrategyDefinitionError::Invalid(
                    "universe instrument asset must not be empty".into(),
                ));
            }
            if !unique.insert(instrument.clone()) {
                return Err(StrategyDefinitionError::Invalid(format!(
                    "duplicate universe instrument {}:{}",
                    venue_label(instrument.venue),
                    instrument.asset
                )));
            }
        }
        Ok(instruments)
    }
}

impl StrategyDefinition {
    pub fn from_toml_str(input: &str) -> Result<Self, StrategyDefinitionError> {
        let definition: Self = toml::from_str(input)?;
        definition.validate()?;
        Ok(definition)
    }

    pub fn validate(&self) -> Result<(), StrategyDefinitionError> {
        if self.schema_version != "strategy.v1" {
            return Err(StrategyDefinitionError::Invalid(format!(
                "unsupported schema_version {}; expected strategy.v1",
                self.schema_version
            )));
        }
        if self.strategy.id.trim().is_empty() {
            return Err(StrategyDefinitionError::Invalid(
                "strategy.id must not be empty".into(),
            ));
        }
        let quantity = Decimal::from_str(self.strategy.order_quantity.trim()).map_err(|error| {
            StrategyDefinitionError::Invalid(format!("invalid order_quantity: {error}"))
        })?;
        if quantity <= Decimal::ZERO {
            return Err(StrategyDefinitionError::Invalid(
                "order_quantity must be positive".into(),
            ));
        }
        self.universe.resolved_instruments()?;
        self.resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        self.policy
            .validate()
            .map_err(StrategyDefinitionError::Invalid)?;
        Ok(())
    }

    pub fn build_instances(&self) -> Result<Vec<AutomatedStrategy>, StrategyDefinitionError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        self.validate()?;
        let order_quantity =
            Decimal::from_str(self.strategy.order_quantity.trim()).map_err(|error| {
                StrategyDefinitionError::Invalid(format!("invalid order_quantity: {error}"))
            })?;
        let automation = self
            .resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        let identities = self.resolved_instance_identities()?;
        let mut strategies = Vec::with_capacity(identities.len());
        for (strategy_id, instrument) in identities {
            let config = StrategyConfig {
                strategy_id,
                asset: instrument.asset,
                venue: instrument.venue,
                order_quantity,
                entry_score: self.strategy.entry_score,
                exit_score: self.strategy.exit_score,
            };
            let instance = AutomatedStrategy::new(config, automation.clone())
                .map_err(|error| StrategyDefinitionError::Invalid(error.into()))?;
            strategies.push(instance);
        }
        Ok(strategies)
    }

    pub fn build_policy_instances(&self) -> Result<Vec<PolicyInstance>, StrategyDefinitionError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        self.validate()?;
        let engine = self
            .policy
            .compile()
            .map_err(StrategyDefinitionError::Invalid)?;
        Ok(self
            .resolved_instance_identities()?
            .into_iter()
            .map(|(strategy_id, instrument)| PolicyInstance {
                strategy_id,
                instrument,
                engine: engine.clone(),
            })
            .collect())
    }

    pub fn build_policy_engines(
        &self,
    ) -> Result<Vec<(AssetKey, PolicyEngine)>, StrategyDefinitionError> {
        Ok(self
            .build_policy_instances()?
            .into_iter()
            .map(|instance| (instance.instrument, instance.engine))
            .collect())
    }

    pub fn resolved_automation(&self) -> Result<StrategyAutomationConfig, String> {
        let mut config = StrategyAutomationConfig::default();
        self.automation.factors.apply(&mut config.factors);
        if let Some(value) = self.automation.candle_interval_ns {
            config.candle_interval_ns = value;
        }
        if let Some(value) = self.automation.signal_horizon_ms {
            config.signal_horizon_ms = value;
        }
        if let Some(value) = self.automation.signal_ttl_ms {
            config.signal_ttl_ms = value;
        }
        if let Some(value) = self.automation.min_confidence {
            config.min_confidence = value;
        }
        if let Some(value) = self.automation.min_emit_interval_ns {
            config.min_emit_interval_ns = value;
        }
        if let Some(value) = &self.automation.alpha_id {
            config.alpha_id = value.clone();
        }
        config.entry_filter = self.automation.entry_filter.clone();
        config.validate().map_err(str::to_owned)?;
        Ok(config)
    }

    fn resolved_instance_identities(
        &self,
    ) -> Result<Vec<(String, AssetKey)>, StrategyDefinitionError> {
        let instruments = self.universe.resolved_instruments()?;
        let multi = instruments.len() > 1;
        let multi_venue = instruments
            .iter()
            .map(|instrument| instrument.venue)
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        Ok(instruments
            .into_iter()
            .map(|instrument| {
                let strategy_id = if multi_venue {
                    format!(
                        "{}:{}:{}",
                        self.strategy.id,
                        venue_label(instrument.venue),
                        instrument.asset
                    )
                } else if multi {
                    format!("{}:{}", self.strategy.id, instrument.asset)
                } else {
                    self.strategy.id.clone()
                };
                (strategy_id, instrument)
            })
            .collect())
    }
}

impl FactorOverrides {
    fn apply(&self, config: &mut FactorConfig) {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    config.$field = value;
                }
            };
        }
        apply!(trade_window);
        apply!(candle_window);
        apply!(book_levels);
        apply!(min_trade_samples);
        apply!(min_candle_samples);
        apply!(max_spread_bps);
        apply!(volatility_soft_cap_bps);
        apply!(vwap_scale_bps);
        apply!(momentum_scale_bps);
        apply!(vwap_weight);
        apply!(trade_imbalance_weight);
        apply!(book_imbalance_weight);
        apply!(momentum_weight);
    }
}

fn venue_label(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "BINANCE_PM",
        Venue::Hyperliquid => "HYPERLIQUID",
        Venue::InteractiveBrokers => "IBKR",
    }
}

fn default_schema_version() -> String {
    "strategy.v1".into()
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_definition_builds_multi_asset_instances() {
        let definition = StrategyDefinition::from_toml_str(
            r#"
                schema_version = "strategy.v1"
                enabled = true

                [universe]
                venue = "HYPERLIQUID"
                assets = ["HYPE", "SOL"]

                [strategy]
                id = "momentum-volume-vwap"
                order_quantity = "1.5"
                entry_score = 0.35
                exit_score = 0.05

                [automation]
                min_confidence = 0.50

                [automation.factors]
                momentum_weight = 0.40

                [automation.entry_filter]
                enabled = true
                min_momentum_bps = 35.0
                min_volume_ratio = 1.5
            "#,
        )
        .unwrap();
        let instances = definition.build_instances().unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(
            instances[0].machine.config.strategy_id,
            "momentum-volume-vwap:HYPE"
        );
        assert_eq!(
            instances[1].machine.config.strategy_id,
            "momentum-volume-vwap:SOL"
        );
    }

    #[test]
    fn standard_universe_can_expand_one_template_across_venues() {
        let definition = StrategyDefinition::from_toml_str(
            r#"
                schema_version = "strategy.v1"

                [universe]
                [[universe.instruments]]
                venue = "BINANCE_PM"
                asset = "ETHUSDT"
                [[universe.instruments]]
                venue = "HYPERLIQUID"
                asset = "HYPE"
                [[universe.instruments]]
                venue = "IBKR"
                asset = "AAPL"

                [strategy]
                id = "portable-momentum"
                order_quantity = "1"
                entry_score = 0.35
                exit_score = 0.05

                [[policy.entries]]
                id = "positive_momentum"
                side = "Buy"
                mode = "all"

                [[policy.entries.predicates]]
                feature = "momentum_bps"
                op = "gte"
                value = 20.0
            "#,
        )
        .unwrap();
        let instances = definition.build_instances().unwrap();
        let policies = definition.build_policy_instances().unwrap();
        assert_eq!(instances.len(), 3);
        assert_eq!(policies.len(), 3);
        assert_eq!(
            instances[0].machine.config.strategy_id,
            "portable-momentum:BINANCE_PM:ETHUSDT"
        );
        assert_eq!(
            policies[0].strategy_id,
            "portable-momentum:BINANCE_PM:ETHUSDT"
        );
        assert_eq!(
            instances[1].machine.config.strategy_id,
            "portable-momentum:HYPERLIQUID:HYPE"
        );
        assert_eq!(
            instances[2].machine.config.strategy_id,
            "portable-momentum:IBKR:AAPL"
        );
    }
}
