//! Venue-agnostic strategy policy kernel.
//!
//! The policy layer consumes normalized domain state only. Binance, Hyperliquid and
//! IBKR SDK types must never leak into this module. A strategy instance may therefore
//! reuse the same filters, entries, exits and sizing rules across venues.

pub mod entries;
pub mod exits;
pub mod factors;
pub mod filters;
pub mod sizing;

use std::collections::BTreeMap;

use pg_types::AssetKey;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeatureFrame {
    #[serde(default)]
    values: BTreeMap<String, f64>,
}

impl FeatureFrame {
    pub fn insert(&mut self, name: impl Into<String>, value: f64) {
        if value.is_finite() {
            self.values.insert(name.into(), value);
        }
    }

    pub fn get(&self, name: &str) -> Option<f64> {
        self.values.get(name).copied()
    }

    pub fn values(&self) -> &BTreeMap<String, f64> {
        &self.values
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
    pub feature: String,
    pub op: ComparisonOp,
    pub value: f64,
}

impl Predicate {
    pub fn evaluate(&self, features: &FeatureFrame) -> Option<bool> {
        let actual = features.get(&self.feature)?;
        Some(match self.op {
            ComparisonOp::Gt => actual > self.value,
            ComparisonOp::Gte => actual >= self.value,
            ComparisonOp::Lt => actual < self.value,
            ComparisonOp::Lte => actual <= self.value,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEvaluation {
    pub matched: bool,
    pub missing_features: Vec<String>,
}

pub fn evaluate_predicates(
    predicates: &[Predicate],
    mode: MatchMode,
    features: &FeatureFrame,
) -> RuleEvaluation {
    if predicates.is_empty() {
        return RuleEvaluation {
            matched: false,
            missing_features: Vec::new(),
        };
    }

    let mut missing_features = Vec::new();
    let mut results = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        match predicate.evaluate(features) {
            Some(value) => results.push(value),
            None => missing_features.push(predicate.feature.clone()),
        }
    }

    // Partial feature state is never enough to create exposure.
    let matched = missing_features.is_empty()
        && match mode {
            MatchMode::All => results.iter().all(|value| *value),
            MatchMode::Any => results.iter().any(|value| *value),
        };

    RuleEvaluation {
        matched,
        missing_features,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionView {
    pub net_quantity: Decimal,
    #[serde(default)]
    pub average_entry_price: Option<Decimal>,
    #[serde(default)]
    pub filled_entries: u32,
    #[serde(default)]
    pub unrealized_return: Option<f64>,
    #[serde(default)]
    pub peak_return: Option<f64>,
}

impl Default for PositionView {
    fn default() -> Self {
        Self {
            net_quantity: Decimal::ZERO,
            average_entry_price: None,
            filled_entries: 0,
            unrealized_return: None,
            peak_return: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StrategyContext<'a> {
    pub instrument: &'a AssetKey,
    pub features: &'a FeatureFrame,
    pub position: &'a PositionView,
    pub now_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::Venue;

    #[test]
    fn missing_feature_fails_closed() {
        let frame = FeatureFrame::default();
        let result = evaluate_predicates(
            &[Predicate {
                feature: "momentum_bps".into(),
                op: ComparisonOp::Gte,
                value: 20.0,
            }],
            MatchMode::All,
            &frame,
        );
        assert!(!result.matched);
        assert_eq!(result.missing_features, vec!["momentum_bps"]);
    }

    #[test]
    fn context_is_identical_across_supported_venues() {
        let features = FeatureFrame::default();
        let position = PositionView::default();
        for key in [
            AssetKey::new(Venue::BinancePm, "ETHUSDT"),
            AssetKey::new(Venue::Hyperliquid, "HYPE"),
            AssetKey::new(Venue::InteractiveBrokers, "AAPL"),
        ] {
            let context = StrategyContext {
                instrument: &key,
                features: &features,
                position: &position,
                now_ns: 1,
            };
            assert_eq!(context.instrument, &key);
        }
    }
}
