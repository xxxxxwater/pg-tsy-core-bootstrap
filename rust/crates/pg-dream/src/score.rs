use crate::ExperimentOutcome;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct EvaluationConstraints {
    pub min_sharpe: Option<f64>,
    pub max_drawdown_pct: Option<f64>,
    pub min_trades: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreWeights {
    pub return_weight: f64,
    pub sharpe_weight: f64,
    pub drawdown_penalty: f64,
    pub execution_cost_penalty: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            return_weight: 1.0,
            sharpe_weight: 1.0,
            drawdown_penalty: 1.0,
            execution_cost_penalty: 0.01,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeScore {
    pub feasible: bool,
    pub utility: f64,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Evaluator {
    constraints: EvaluationConstraints,
    weights: ScoreWeights,
}

impl Evaluator {
    pub fn new(
        constraints: EvaluationConstraints,
        weights: ScoreWeights,
    ) -> Result<Self, EvaluationError> {
        validate_constraints(constraints)?;
        validate_weights(weights)?;
        Ok(Self {
            constraints,
            weights,
        })
    }

    pub fn constraints(&self) -> EvaluationConstraints {
        self.constraints
    }

    pub fn weights(&self) -> ScoreWeights {
        self.weights
    }

    pub fn score(&self, outcome: &ExperimentOutcome) -> Result<NodeScore, EvaluationError> {
        validate_outcome(outcome)?;

        let metrics = &outcome.metrics;
        let utility = self.weights.return_weight * metrics.net_return_pct
            + self.weights.sharpe_weight * metrics.sharpe
            - self.weights.drawdown_penalty * metrics.max_drawdown_pct
            - self.weights.execution_cost_penalty * metrics.execution_cost_bps;
        if !utility.is_finite() {
            return Err(EvaluationError::NonFinite("utility"));
        }

        let mut violations = Vec::new();
        if !outcome.success {
            violations.push("experiment did not complete successfully".into());
        }
        if let Some(min_sharpe) = self.constraints.min_sharpe
            && metrics.sharpe < min_sharpe
        {
            violations.push(format!(
                "sharpe {} is below minimum {}",
                metrics.sharpe, min_sharpe
            ));
        }
        if let Some(max_drawdown_pct) = self.constraints.max_drawdown_pct
            && metrics.max_drawdown_pct > max_drawdown_pct
        {
            violations.push(format!(
                "max drawdown {} exceeds limit {}",
                metrics.max_drawdown_pct, max_drawdown_pct
            ));
        }
        if let Some(min_trades) = self.constraints.min_trades
            && metrics.trades < min_trades
        {
            violations.push(format!(
                "trade count {} is below minimum {}",
                metrics.trades, min_trades
            ));
        }

        Ok(NodeScore {
            feasible: violations.is_empty(),
            utility,
            violations,
        })
    }

    pub fn is_better(&self, candidate: &NodeScore, current: &NodeScore) -> bool {
        match (candidate.feasible, current.feasible) {
            (true, false) => true,
            (false, true) => false,
            _ => candidate.utility.total_cmp(&current.utility).is_gt(),
        }
    }
}


fn validate_constraints(constraints: EvaluationConstraints) -> Result<(), EvaluationError> {
    if let Some(value) = constraints.min_sharpe {
        ensure_finite("min_sharpe", value)?;
    }
    if let Some(value) = constraints.max_drawdown_pct {
        ensure_finite("max_drawdown_pct", value)?;
        if value < 0.0 {
            return Err(EvaluationError::InvalidRange("max_drawdown_pct"));
        }
    }
    Ok(())
}

fn validate_weights(weights: ScoreWeights) -> Result<(), EvaluationError> {
    ensure_finite("return_weight", weights.return_weight)?;
    ensure_finite("sharpe_weight", weights.sharpe_weight)?;
    ensure_finite("drawdown_penalty", weights.drawdown_penalty)?;
    ensure_finite("execution_cost_penalty", weights.execution_cost_penalty)?;

    if weights.drawdown_penalty < 0.0 {
        return Err(EvaluationError::InvalidRange("drawdown_penalty"));
    }
    if weights.execution_cost_penalty < 0.0 {
        return Err(EvaluationError::InvalidRange("execution_cost_penalty"));
    }
    Ok(())
}

fn validate_outcome(outcome: &ExperimentOutcome) -> Result<(), EvaluationError> {
    let metrics = &outcome.metrics;
    ensure_finite("net_return_pct", metrics.net_return_pct)?;
    ensure_finite("sharpe", metrics.sharpe)?;
    ensure_finite("max_drawdown_pct", metrics.max_drawdown_pct)?;
    ensure_finite("turnover", metrics.turnover)?;
    ensure_finite("execution_cost_bps", metrics.execution_cost_bps)?;

    if metrics.max_drawdown_pct < 0.0 {
        return Err(EvaluationError::InvalidRange("max_drawdown_pct"));
    }
    if metrics.turnover < 0.0 {
        return Err(EvaluationError::InvalidRange("turnover"));
    }
    if metrics.execution_cost_bps < 0.0 {
        return Err(EvaluationError::InvalidRange("execution_cost_bps"));
    }
    Ok(())
}

fn ensure_finite(field: &'static str, value: f64) -> Result<(), EvaluationError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(EvaluationError::NonFinite(field))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvaluationError {
    #[error("{0} must be finite")]
    NonFinite(&'static str),
    #[error("{0} is outside the accepted range")]
    InvalidRange(&'static str),
}
