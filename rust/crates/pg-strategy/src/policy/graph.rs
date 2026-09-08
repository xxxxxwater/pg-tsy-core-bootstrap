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
        for (kind, id, predicate_count) in self
            .filters
            .iter()
            .map(|rule| ("filter", rule.id.as_str(), rule.predicates.len()))
            .chain(
                self.entries
                    .iter()
                    .map(|rule| ("entry", rule.id.as_str(), rule.predicates.len())),
            )
            .chain(
                self.exits
                    .iter()
                    .map(|rule| ("exit", rule.id.as_str(), rule.predicates.len())),
            )
        {
            if id.trim().is_empty() {
                return Err(format!("{kind} rule id must not be empty"));
            }
            if predicate_count == 0 {
                return Err(format!("{kind} rule {id} must have at least one predicate"));
            }
            if !ids.insert(id.to_owned()) {
                return Err(format!("duplicate policy rule id {id}"));
            }
        }
        Ok(())
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
        features.insert("unrealized_return", -0.15);
        let position = PositionView::default();
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
}
