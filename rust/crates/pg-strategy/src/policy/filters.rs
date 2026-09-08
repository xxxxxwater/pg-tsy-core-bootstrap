use serde::{Deserialize, Serialize};

use super::{MatchMode, Predicate, RuleEvaluation, StrategyContext, evaluate_predicates};

pub trait StrategyFilter: Send + Sync {
    fn id(&self) -> &str;
    fn evaluate(&self, context: &StrategyContext<'_>) -> RuleEvaluation;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclarativeFilter {
    pub id: String,
    #[serde(default)]
    pub mode: MatchMode,
    #[serde(default)]
    pub predicates: Vec<Predicate>,
}

impl StrategyFilter for DeclarativeFilter {
    fn id(&self) -> &str {
        &self.id
    }

    fn evaluate(&self, context: &StrategyContext<'_>) -> RuleEvaluation {
        evaluate_predicates(&self.predicates, self.mode, context)
    }
}
