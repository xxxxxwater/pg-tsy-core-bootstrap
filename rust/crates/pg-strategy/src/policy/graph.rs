use std::collections::BTreeSet;

use pg_types::Side;
use serde::{Deserialize, Serialize};

use super::StrategyContext;
use super::entries::{DeclarativeEntryRule, EntryRule};
use super::exits::{DeclarativeExitRule, ExitRule};
use super::filters::{DeclarativeFilter, StrategyFilter};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyDefinition {
    #[serde(default)]
    pub filters: Vec<DeclarativeFilter>,
    #[serde(default)]
    pub entries: Vec<DeclarativeEntryRule>,
    #[serde(default)]
    pub exits: Vec<DeclarativeExitRule>,
}

impl PolicyDefinition {
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = BTreeSet::new();
        for (kind, id, predicates) in self
            .filters
            .iter()
            .map(|rule| ("filter", rule.id.as_str(), rule.predicates.as_slice()))
            .chain(
                self.entries
                    .iter()
                    .map(|rule| ("entry", rule.id.as_str(), rule.predicates.as_slice())),
            )
            .chain(
                self.exits
                    .iter()
                    .map(|rule| ("exit", rule.id.as_str(), rule.predicates.as_slice())),
            )
        {
            if id.trim().is_empty() {
                return Err(format!("{kind} rule id must not be empty"));
            }
            if predicates.is_empty() {
                return Err(format!("{kind} rule {id} must have at least one predicate"));
            }
            if !ids.insert(id.to_owned()) {
                return Err(format!("duplicate policy rule id {id}"));
            }
            for predicate in predicates {
                if predicate.feature.trim().is_empty() {
                    return Err(format!("{kind} rule {id} has an empty feature name"));
                }
                if !predicate.value.is_finite() {
                    return Err(format!(
                        "{kind} rule {id} predicate {} has a non-finite value",
                        predicate.feature
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn required_features(&self) -> BTreeSet<String> {
        self.filters
            .iter()
            .flat_map(|rule| rule.predicates.iter())
            .chain(self.entries.iter().flat_map(|rule| rule.predicates.iter()))
            .chain(self.exits.iter().flat_map(|rule| rule.predicates.iter()))
            .map(|predicate| predicate.feature.clone())
            .collect()
    }

    pub fn compile(&self) -> Result<PolicyEngine, String> {
        self.validate()?;
        Ok(PolicyEngine {
            definition: self.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    definition: PolicyDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryPolicyDecision {
    NoMatch,
    Blocked {
        filter_id: String,
        missing_features: Vec<String>,
    },
    Matched {
        rule_id: String,
        side: Side,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitPolicyDecision {
    NoMatch,
    Matched { rule_id: String },
}

impl PolicyEngine {
    pub fn evaluate_entry(&self, context: &StrategyContext<'_>) -> EntryPolicyDecision {
        for filter in &self.definition.filters {
            let result = filter.evaluate(context);
            if !result.matched {
                return EntryPolicyDecision::Blocked {
                    filter_id: filter.id().to_owned(),
                    missing_features: result.missing_features,
                };
            }
        }

        for rule in &self.definition.entries {
            if rule.evaluate(context).matched {
                return EntryPolicyDecision::Matched {
                    rule_id: rule.id().to_owned(),
                    side: rule.side(),
                };
            }
        }
        EntryPolicyDecision::NoMatch
    }

    pub fn evaluate_exit(&self, context: &StrategyContext<'_>) -> ExitPolicyDecision {
        // Exit evaluation intentionally ignores entry filters. Once a strategy owns
        // exposure, a weak entry screen must never disable position management.
        for rule in &self.definition.exits {
            if rule.evaluate(context).matched {
                return ExitPolicyDecision::Matched {
                    rule_id: rule.id().to_owned(),
                };
            }
        }
        ExitPolicyDecision::NoMatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ComparisonOp, FeatureFrame, MatchMode, PositionView, Predicate};
    use pg_types::{AssetKey, Venue};

    fn predicate(feature: &str, op: ComparisonOp, value: f64) -> Predicate {
        Predicate {
            feature: feature.into(),
            op,
            value,
        }
    }

    #[test]
    fn failed_entry_filter_does_not_disable_exit() {
        let engine = PolicyDefinition {
            filters: vec![DeclarativeFilter {
                id: "liquidity".into(),
                mode: MatchMode::All,
                predicates: vec![predicate("spread_bps", ComparisonOp::Lte, 20.0)],
            }],
            entries: vec![DeclarativeEntryRule {
                id: "momentum".into(),
                side: Side::Buy,
                mode: MatchMode::All,
                predicates: vec![predicate("momentum_bps", ComparisonOp::Gte, 20.0)],
            }],
            exits: vec![DeclarativeExitRule {
                id: "stop".into(),
                mode: MatchMode::All,
                predicates: vec![predicate("unrealized_return", ComparisonOp::Lte, -0.10)],
            }],
        }
        .compile()
        .unwrap();

        let mut features = FeatureFrame::default();
        features.insert("spread_bps", 50.0);
        features.insert("momentum_bps", 40.0);
        let position = PositionView {
            unrealized_return: Some(-0.15),
            ..PositionView::default()
        };
        let instrument = AssetKey::new(Venue::Hyperliquid, "HYPE");
        let context = StrategyContext {
            instrument: &instrument,
            features: &features,
            position: &position,
            now_ns: 1,
        };

        assert!(matches!(
            engine.evaluate_entry(&context),
            EntryPolicyDecision::Blocked { .. }
        ));
        assert_eq!(
            engine.evaluate_exit(&context),
            ExitPolicyDecision::Matched {
                rule_id: "stop".into()
            }
        );
    }

    #[test]
    fn required_features_are_deduplicated_across_rule_types() {
        let definition = PolicyDefinition {
            filters: vec![DeclarativeFilter {
                id: "spread".into(),
                mode: MatchMode::All,
                predicates: vec![predicate("spread_bps", ComparisonOp::Lte, 20.0)],
            }],
            entries: vec![DeclarativeEntryRule {
                id: "entry".into(),
                side: Side::Buy,
                mode: MatchMode::All,
                predicates: vec![
                    predicate("spread_bps", ComparisonOp::Lte, 20.0),
                    predicate("momentum_bps", ComparisonOp::Gte, 20.0),
                ],
            }],
            exits: vec![DeclarativeExitRule {
                id: "exit".into(),
                mode: MatchMode::All,
                predicates: vec![predicate("unrealized_return", ComparisonOp::Lte, -0.10)],
            }],
        };
        let features = definition.required_features();
        assert_eq!(features.len(), 3);
        assert!(features.contains("spread_bps"));
        assert!(features.contains("momentum_bps"));
        assert!(features.contains("unrealized_return"));
    }
}
