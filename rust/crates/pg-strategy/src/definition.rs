use std::collections::BTreeSet;
use std::str::FromStr;

use pg_marketdata::{
    FeedSpec, UniverseFilter, candle_interval_supported, default_candle_interval,
    describe_candle_interval, describe_candle_intervals,
};
use pg_types::{AssetKey, Venue};
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

use crate::StrategyConfig;
use crate::automation::{AutomatedStrategy, StrategyAutomationConfig};
use crate::factors::FactorConfig;
use crate::policy::graph::{PolicyDefinition, PolicyEngine};
use crate::policy::providers::{FeaturePlan, FeatureProviderRegistry, LiveFeatureEngine};
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
    /// Standard multi-venue static form. Each strategy instance owns one AssetKey.
    #[serde(default)]
    pub instruments: Vec<InstrumentDefinition>,
    /// Production discovery form. A dynamic template contains no hard-coded assets;
    /// runtime discovery resolves these sources into concrete strategy instances.
    #[serde(default)]
    pub dynamic: Vec<DynamicUniverseDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentDefinition {
    pub venue: Venue,
    pub asset: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DynamicUniverseDefinition {
    pub venue: Venue,
    #[serde(default = "default_dynamic_top_n")]
    pub top_n: usize,
    pub min_day_notional_volume: Option<String>,
    pub min_open_interest: Option<String>,
    pub max_spread_bps: Option<String>,
    pub min_price: Option<String>,
    pub max_price: Option<String>,
    #[serde(default)]
    pub include_symbols: Vec<String>,
    #[serde(default)]
    pub exclude_symbols: Vec<String>,
}

impl DynamicUniverseDefinition {
    pub fn filter(&self) -> Result<UniverseFilter, StrategyDefinitionError> {
        if self.top_n == 0 || self.top_n > 500 {
            return Err(StrategyDefinitionError::Invalid(format!(
                "dynamic universe top_n for {:?} must be in [1, 500]",
                self.venue
            )));
        }
        if self.venue == Venue::InteractiveBrokers && self.top_n > 50 {
            return Err(StrategyDefinitionError::Invalid(
                "IBKR TWS scanner-backed dynamic universe top_n must be <= 50".into(),
            ));
        }
        Ok(UniverseFilter {
            min_day_notional_volume: decimal_option(
                "min_day_notional_volume",
                self.min_day_notional_volume.as_deref(),
            )?,
            min_open_interest: decimal_option(
                "min_open_interest",
                self.min_open_interest.as_deref(),
            )?,
            max_spread_bps: decimal_option("max_spread_bps", self.max_spread_bps.as_deref())?,
            min_price: decimal_option("min_price", self.min_price.as_deref())?,
            max_price: decimal_option("max_price", self.max_price.as_deref())?,
            include_symbols: normalized_symbols(&self.include_symbols)?,
            exclude_symbols: normalized_symbols(&self.exclude_symbols)?,
            top_n: Some(self.top_n),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyTemplateDefinition {
    pub id: String,
    pub order_quantity: String,
    pub entry_score: f64,
    pub exit_score: f64,
    /// Opt in to short entries. Absent means long-only, which is the safe default
    /// for the equity/IBKR side and for any strategy whose score is symmetric.
    #[serde(default)]
    pub allow_short: bool,
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

pub struct PolicyInstance {
    pub strategy_id: String,
    pub instrument: AssetKey,
    pub engine: PolicyEngine,
    pub features: LiveFeatureEngine,
    /// Size used when a matched entry rule is turned into an order intent.
    pub order_quantity: Decimal,
    /// Mirrors StrategyConfig::allow_short for the portable policy path.
    pub allow_short: bool,
}

impl PolicyInstance {
    /// Whether this instance is driven by a portable rule graph rather than by the
    /// legacy score-threshold automation path.
    pub fn is_policy_driven(&self) -> bool {
        self.engine.is_defined()
    }

    pub fn subscriptions(&self) -> Vec<FeedSpec> {
        self.features.subscriptions(&self.instrument)
    }
}

impl UniverseDefinition {
    pub fn is_dynamic(&self) -> bool {
        !self.dynamic.is_empty()
    }

    pub fn dynamic_sources(&self) -> &[DynamicUniverseDefinition] {
        &self.dynamic
    }

    fn validate_shape(&self) -> Result<(), StrategyDefinitionError> {
        let using_legacy = self.venue.is_some() || !self.assets.is_empty();
        let using_standard = !self.instruments.is_empty();
        let using_dynamic = self.is_dynamic();
        let modes = usize::from(using_legacy) + usize::from(using_standard) + usize::from(using_dynamic);
        if modes != 1 {
            return Err(StrategyDefinitionError::Invalid(
                "universe must use exactly one of venue+assets, instruments, or dynamic sources"
                    .into(),
            ));
        }
        if using_dynamic {
            let mut venues = BTreeSet::new();
            for source in &self.dynamic {
                source.filter()?;
                if !venues.insert(source.venue) {
                    return Err(StrategyDefinitionError::Invalid(format!(
                        "duplicate dynamic universe source for {:?}",
                        source.venue
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn resolved_instruments(&self) -> Result<Vec<AssetKey>, StrategyDefinitionError> {
        self.validate_shape()?;
        if self.is_dynamic() {
            return Ok(Vec::new());
        }
        let instruments = if !self.instruments.is_empty() {
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
                    "universe.assets must not be empty".into(),
                ));
            }
            self.assets
                .iter()
                .map(|asset| AssetKey::new(venue, asset.trim()))
                .collect::<Vec<_>>()
        };
        validate_instruments(&instruments)?;
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
        self.universe.validate_shape()?;
        if !self.universe.is_dynamic() {
            self.universe.resolved_instruments()?;
        }
        self.policy
            .validate()
            .map_err(StrategyDefinitionError::Invalid)?;
        Ok(())
    }

    pub fn is_dynamic(&self) -> bool {
        self.universe.is_dynamic()
    }

    /// Reject a candle resolution the selected venues cannot serve.
    fn validate_candle_contract_for(
        &self,
        instruments: &[AssetKey],
    ) -> Result<(), StrategyDefinitionError> {
        let Some(configured) = self.automation.candle_interval_ns else {
            return Ok(());
        };
        for instrument in instruments {
            if !candle_interval_supported(instrument.venue, configured) {
                return Err(StrategyDefinitionError::Invalid(format!(
                    "venue {:?} cannot serve candle interval {} ({} ns); supported: {}",
                    instrument.venue,
                    describe_candle_interval(configured),
                    configured,
                    describe_candle_intervals(instrument.venue),
                )));
            }
        }
        Ok(())
    }

    pub fn build_instances(&self) -> Result<Vec<AutomatedStrategy>, StrategyDefinitionError> {
        if !self.enabled || self.is_dynamic() {
            return Ok(Vec::new());
        }
        let instruments = self.universe.resolved_instruments()?;
        self.build_instances_from(&instruments, false)
    }

    pub fn build_instances_for(
        &self,
        instruments: &[AssetKey],
    ) -> Result<Vec<AutomatedStrategy>, StrategyDefinitionError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        if !self.is_dynamic() {
            return Err(StrategyDefinitionError::Invalid(
                "build_instances_for requires a dynamic universe template".into(),
            ));
        }
        self.validate_dynamic_selection(instruments)?;
        self.build_instances_from(instruments, true)
    }

    fn build_instances_from(
        &self,
        instruments: &[AssetKey],
        force_scoped_identity: bool,
    ) -> Result<Vec<AutomatedStrategy>, StrategyDefinitionError> {
        self.validate()?;
        validate_instruments(instruments)?;
        self.validate_candle_contract_for(instruments)?;
        let order_quantity =
            Decimal::from_str(self.strategy.order_quantity.trim()).map_err(|error| {
                StrategyDefinitionError::Invalid(format!("invalid order_quantity: {error}"))
            })?;
        let automation = self
            .resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        let identities = self.instance_identities(instruments, force_scoped_identity);
        let mut strategies = Vec::with_capacity(identities.len());
        for (strategy_id, instrument) in identities {
            let venue = instrument.venue;
            let config = StrategyConfig {
                strategy_id,
                asset: instrument.asset,
                venue,
                order_quantity,
                entry_score: self.strategy.entry_score,
                exit_score: self.strategy.exit_score,
                allow_short: self.strategy.allow_short,
            };
            let mut automation = automation.clone();
            automation.candle_interval_ns = self.resolved_candle_interval(venue);
            let instance = AutomatedStrategy::new(config, automation)
                .map_err(|error| StrategyDefinitionError::Invalid(error.into()))?;
            strategies.push(instance);
        }
        Ok(strategies)
    }

    fn resolved_candle_interval(&self, venue: Venue) -> u64 {
        self.automation
            .candle_interval_ns
            .unwrap_or_else(|| default_candle_interval(venue))
    }

    pub fn policy_feature_plan(&self) -> Result<FeaturePlan, StrategyDefinitionError> {
        FeatureProviderRegistry::standard()
            .plan(&self.policy.required_features())
            .map_err(StrategyDefinitionError::Invalid)
    }

    pub fn build_policy_instances(&self) -> Result<Vec<PolicyInstance>, StrategyDefinitionError> {
        if !self.enabled || self.is_dynamic() {
            return Ok(Vec::new());
        }
        let instruments = self.universe.resolved_instruments()?;
        self.build_policy_instances_from(&instruments, false)
    }

    pub fn build_policy_instances_for(
        &self,
        instruments: &[AssetKey],
    ) -> Result<Vec<PolicyInstance>, StrategyDefinitionError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        if !self.is_dynamic() {
            return Err(StrategyDefinitionError::Invalid(
                "build_policy_instances_for requires a dynamic universe template".into(),
            ));
        }
        self.validate_dynamic_selection(instruments)?;
        self.build_policy_instances_from(instruments, true)
    }

    fn build_policy_instances_from(
        &self,
        instruments: &[AssetKey],
        force_scoped_identity: bool,
    ) -> Result<Vec<PolicyInstance>, StrategyDefinitionError> {
        self.validate()?;
        validate_instruments(instruments)?;
        self.validate_candle_contract_for(instruments)?;
        let engine = self
            .policy
            .compile()
            .map_err(StrategyDefinitionError::Invalid)?;
        let plan = self.policy_feature_plan()?;
        let order_quantity =
            Decimal::from_str(self.strategy.order_quantity.trim()).map_err(|error| {
                StrategyDefinitionError::Invalid(format!("invalid order_quantity: {error}"))
            })?;
        let automation = self
            .resolved_automation()
            .map_err(StrategyDefinitionError::Invalid)?;
        let identities = self.instance_identities(instruments, force_scoped_identity);
        let mut instances = Vec::with_capacity(identities.len());
        for (strategy_id, instrument) in identities {
            let candle_interval_ns = self.resolved_candle_interval(instrument.venue);
            let features = LiveFeatureEngine::new(
                plan.clone(),
                automation.factors.clone(),
                candle_interval_ns,
                automation.entry_filter.volume_window,
                automation.entry_filter.min_volume_samples,
            )
            .map_err(StrategyDefinitionError::Invalid)?;
            instances.push(PolicyInstance {
                strategy_id,
                instrument,
                engine: engine.clone(),
                features,
                order_quantity,
                allow_short: self.strategy.allow_short,
            });
        }
        Ok(instances)
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

    fn validate_dynamic_selection(
        &self,
        instruments: &[AssetKey],
    ) -> Result<(), StrategyDefinitionError> {
        validate_instruments(instruments)?;
        let allowed = self
            .universe
            .dynamic_sources()
            .iter()
            .map(|source| source.venue)
            .collect::<BTreeSet<_>>();
        for instrument in instruments {
            if !allowed.contains(&instrument.venue) {
                return Err(StrategyDefinitionError::Invalid(format!(
                    "dynamic selection contains venue {:?} not declared by template",
                    instrument.venue
                )));
            }
        }
        Ok(())
    }

    fn instance_identities(
        &self,
        instruments: &[AssetKey],
        force_scoped_identity: bool,
    ) -> Vec<(String, AssetKey)> {
        let multi = instruments.len() > 1;
        let multi_venue = instruments
            .iter()
            .map(|instrument| instrument.venue)
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        instruments
            .iter()
            .cloned()
            .map(|instrument| {
                let strategy_id = if force_scoped_identity || multi_venue {
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
            .collect()
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

fn validate_instruments(instruments: &[AssetKey]) -> Result<(), StrategyDefinitionError> {
    if instruments.is_empty() {
        return Err(StrategyDefinitionError::Invalid(
            "universe must contain at least one instrument".into(),
        ));
    }
    let mut unique = BTreeSet::new();
    for instrument in instruments {
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
    Ok(())
}

fn normalized_symbols(values: &[String]) -> Result<BTreeSet<String>, StrategyDefinitionError> {
    let mut symbols = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            return Err(StrategyDefinitionError::Invalid(
                "dynamic universe symbol filters cannot contain empty values".into(),
            ));
        }
        symbols.insert(value.to_owned());
    }
    Ok(symbols)
}

fn decimal_option(
    field: &str,
    value: Option<&str>,
) -> Result<Option<Decimal>, StrategyDefinitionError> {
    value
        .map(|value| {
            Decimal::from_str(value.trim()).map_err(|error| {
                StrategyDefinitionError::Invalid(format!(
                    "invalid dynamic universe {field}={value}: {error}"
                ))
            })
        })
        .transpose()
}

fn venue_label(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "BINANCE_PM",
        Venue::Hyperliquid => "HYPERLIQUID",
        Venue::InteractiveBrokers => "IBKR",
    }
}

fn default_dynamic_top_n() -> usize {
    20
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
    use crate::policy::providers::FeedRequirement;

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
    fn dynamic_definition_needs_no_static_asset_and_builds_stable_scoped_ids() {
        let definition = StrategyDefinition::from_toml_str(
            r#"
                schema_version = "strategy.v1"
                enabled = true

                [universe]
                [[universe.dynamic]]
                venue = "HYPERLIQUID"
                top_n = 10
                min_day_notional_volume = "1000000"

                [[universe.dynamic]]
                venue = "IBKR"
                top_n = 5
                min_price = "5"

                [strategy]
                id = "dynamic-momentum"
                order_quantity = "1"
                entry_score = 0.35
                exit_score = 0.05
            "#,
        )
        .unwrap();
        assert!(definition.is_dynamic());
        assert!(definition.build_instances().unwrap().is_empty());
        let instances = definition
            .build_instances_for(&[
                AssetKey::new(Venue::Hyperliquid, "SOL"),
                AssetKey::new(Venue::InteractiveBrokers, "NVDA"),
            ])
            .unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(
            instances[0].machine.config.strategy_id,
            "dynamic-momentum:HYPERLIQUID:SOL"
        );
        assert_eq!(
            instances[1].machine.config.strategy_id,
            "dynamic-momentum:IBKR:NVDA"
        );
        assert_eq!(
            definition.universe.dynamic_sources()[0].filter().unwrap().top_n,
            Some(10)
        );
    }

    #[test]
    fn dynamic_and_static_universe_cannot_be_mixed() {
        let error = StrategyDefinition::from_toml_str(
            r#"
                [universe]
                venue = "HYPERLIQUID"
                assets = ["HYPE"]
                [[universe.dynamic]]
                venue = "HYPERLIQUID"
                top_n = 5

                [strategy]
                id = "bad"
                order_quantity = "1"
                entry_score = 0.35
                exit_score = 0.05
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("exactly one"));
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
        let plan = definition.policy_feature_plan().unwrap();
        assert_eq!(plan.feed_requirements().len(), 1);
        assert!(plan.feed_requirements().contains(&FeedRequirement::Candle));
    }
}
