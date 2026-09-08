use std::collections::BTreeSet;
use std::str::FromStr;

use pg_types::Venue;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

use crate::automation::{AutomatedStrategy, StrategyAutomationConfig};
use crate::factors::FactorConfig;
use crate::selector::EntryFilterConfig;
use crate::StrategyConfig;

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct UniverseDefinition {
    pub venue: Venue,
    pub assets: Vec<String>,
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
        if self.universe.assets.is_empty() {
            return Err(StrategyDefinitionError::Invalid(
                "universe.assets must not be empty".into(),
            ));
        }
        let mut assets = BTreeSet::new();
        for asset in &self.universe.assets {
            let asset = asset.trim();
            if asset.is_empty() {
                return Err(StrategyDefinitionError::Invalid(
                    "universe asset must not be empty".into(),
                ));
            }
            if !assets.insert(asset.to_owned()) {
                return Err(StrategyDefinitionError::Invalid(format!(
                    "duplicate universe asset {asset}"
                )));
            }
        }
        self.resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        Ok(())
    }

    pub fn build_instances(&self) -> Result<Vec<AutomatedStrategy>, StrategyDefinitionError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        self.validate()?;
        let order_quantity = Decimal::from_str(self.strategy.order_quantity.trim()).map_err(|error| {
            StrategyDefinitionError::Invalid(format!("invalid order_quantity: {error}"))
        })?;
        let automation = self
            .resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        let multi_asset = self.universe.assets.len() > 1;
        let mut strategies = Vec::with_capacity(self.universe.assets.len());
        for asset in &self.universe.assets {
            let asset = asset.trim();
            let strategy_id = if multi_asset {
                format!("{}:{}", self.strategy.id, asset)
            } else {
                self.strategy.id.clone()
            };
            let config = StrategyConfig {
                strategy_id,
                asset: asset.to_owned(),
                venue: self.universe.venue,
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
    fn partial_toml_definition_builds_multi_asset_instances() {
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
        assert_eq!(instances[0].machine.config.strategy_id, "momentum-volume-vwap:HYPE");
        assert_eq!(instances[1].machine.config.strategy_id, "momentum-volume-vwap:SOL");
    }
}
