use pg_types::Side;
use serde::{Deserialize, Serialize};

use super::{MatchMode, Predicate, RuleEvaluation, StrategyContext, evaluate_predicates};

pub trait EntryRule: Send + Sync {
    fn id(&self) -> &str;
    fn side(&self) -> Side;
    fn evaluate(&self, context: &StrategyContext<'_>) -> RuleEvaluation;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclarativeEntryRule {
    pub id: String,
    pub side: Side,
    #[serde(default)]
    pub mode: MatchMode,
    #[serde(default)]
    pub predicates: Vec<Predicate>,
}

impl EntryRule for DeclarativeEntryRule {
    fn id(&self) -> &str {
        &self.id
    }

    fn side(&self) -> Side {
        self.side
    }

    fn evaluate(&self, context: &StrategyContext<'_>) -> RuleEvaluation {
        evaluate_predicates(&self.predicates, self.mode, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ComparisonOp, FeatureFrame, PositionView};
    use pg_types::{AssetKey, Venue};

    #[test]
    fn same_entry_rule_matches_across_venues() {
        let rule = DeclarativeEntryRule {
            id: "momentum-volume".into(),
            side: Side::Buy,
            mode: MatchMode::All,
            predicates: vec![
                Predicate {
                    feature: "momentum_bps".into(),
                    op: ComparisonOp::Gte,
                    value: 20.0,
                },
                Predicate {
                    feature: "volume_ratio".into(),
                    op: ComparisonOp::Gte,
                    value: 1.25,
                },
            ],
        };
        let mut features = FeatureFrame::default();
        features.insert("momentum_bps", 35.0);
        features.insert("volume_ratio", 1.6);
        let position = PositionView::default();

        for instrument in [
            AssetKey::new(Venue::BinancePm, "ETHUSDT"),
            AssetKey::new(Venue::Hyperliquid, "HYPE"),
            AssetKey::new(Venue::InteractiveBrokers, "AAPL"),
        ] {
            assert!(
                rule.evaluate(&StrategyContext {
                    instrument: &instrument,
                    features: &features,
                    position: &position,
                    now_ns: 1,
                })
                .matched
            );
        }
    }
}
