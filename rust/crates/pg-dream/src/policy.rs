use crate::{NodeId, NodeScore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyCandidate {
    pub node_id: NodeId,
    pub depth: u32,
    pub created_seq: u64,
    pub cost_units: u64,
    pub score: NodeScore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyView {
    pub round: u32,
    pub remaining_cost_units: u64,
    pub stagnation_rounds: u32,
    pub candidates: Vec<PolicyCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyAction {
    pub expand: Vec<NodeId>,
    pub stop: bool,
}

impl PolicyAction {
    pub fn stop() -> Self {
        Self {
            expand: Vec::new(),
            stop: true,
        }
    }
}

pub trait ExplorationPolicy {
    fn select(&self, view: &PolicyView) -> PolicyAction;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplorationPolicyConfig {
    /// Maximum experiment slots selected in one decision round.
    pub worker_limit: usize,
    /// Maximum slots assigned to the same parent in one round.
    pub fanout_per_parent: usize,
    pub depth_penalty: f64,
    pub node_cost_penalty: f64,
    /// Stop after this many rounds without improving the best observed node.
    pub patience_rounds: u32,
    pub max_rounds: u32,
}

impl Default for ExplorationPolicyConfig {
    fn default() -> Self {
        Self {
            worker_limit: 4,
            fanout_per_parent: 1,
            depth_penalty: 0.0,
            node_cost_penalty: 0.0,
            patience_rounds: 3,
            max_rounds: 64,
        }
    }
}

impl ExplorationPolicyConfig {
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.worker_limit == 0 {
            return Err(PolicyError::Invalid("worker_limit must be positive"));
        }
        if self.fanout_per_parent == 0 {
            return Err(PolicyError::Invalid("fanout_per_parent must be positive"));
        }
        if self.fanout_per_parent > self.worker_limit {
            return Err(PolicyError::Invalid(
                "fanout_per_parent cannot exceed worker_limit",
            ));
        }
        if self.patience_rounds == 0 {
            return Err(PolicyError::Invalid("patience_rounds must be positive"));
        }
        if self.max_rounds == 0 {
            return Err(PolicyError::Invalid("max_rounds must be positive"));
        }
        if !self.depth_penalty.is_finite() || self.depth_penalty < 0.0 {
            return Err(PolicyError::Invalid(
                "depth_penalty must be finite and non-negative",
            ));
        }
        if !self.node_cost_penalty.is_finite() || self.node_cost_penalty < 0.0 {
            return Err(PolicyError::Invalid(
                "node_cost_penalty must be finite and non-negative",
            ));
        }
        Ok(())
    }

    /// Deterministic local meta-search around the incumbent policy.
    ///
    /// The incumbent is always first, so DreamEngine cannot select a lower
    /// objective on the same frozen replay worlds.
    pub fn revisions(&self) -> Vec<Self> {
        let mut out = vec![self.clone()];

        if self.worker_limit > 1 {
            let mut candidate = self.clone();
            candidate.worker_limit -= 1;
            candidate.fanout_per_parent =
                candidate.fanout_per_parent.min(candidate.worker_limit);
            push_unique(&mut out, candidate);
        }
        if self.worker_limit < 16 {
            let mut candidate = self.clone();
            candidate.worker_limit += 1;
            push_unique(&mut out, candidate);
        }

        if self.fanout_per_parent > 1 {
            let mut candidate = self.clone();
            candidate.fanout_per_parent -= 1;
            push_unique(&mut out, candidate);
        }
        if self.fanout_per_parent < self.worker_limit {
            let mut candidate = self.clone();
            candidate.fanout_per_parent += 1;
            push_unique(&mut out, candidate);
        }

        for multiplier in [0.5, 2.0] {
            let mut candidate = self.clone();
            candidate.depth_penalty = if self.depth_penalty == 0.0 {
                if multiplier < 1.0 { 0.01 } else { 0.05 }
            } else {
                self.depth_penalty * multiplier
            };
            push_unique(&mut out, candidate);

            let mut candidate = self.clone();
            candidate.node_cost_penalty = if self.node_cost_penalty == 0.0 {
                if multiplier < 1.0 { 0.01 } else { 0.05 }
            } else {
                self.node_cost_penalty * multiplier
            };
            push_unique(&mut out, candidate);
        }

        if self.patience_rounds > 1 {
            let mut candidate = self.clone();
            candidate.patience_rounds -= 1;
            push_unique(&mut out, candidate);
        }
        if self.patience_rounds < 32 {
            let mut candidate = self.clone();
            candidate.patience_rounds += 1;
            push_unique(&mut out, candidate);
        }

        out
    }
}

fn push_unique(out: &mut Vec<ExplorationPolicyConfig>, candidate: ExplorationPolicyConfig) {
    if candidate.validate().is_ok() && !out.contains(&candidate) {
        out.push(candidate);
    }
}

#[derive(Debug, Clone)]
pub struct LinearExplorationPolicy {
    config: ExplorationPolicyConfig,
}

impl LinearExplorationPolicy {
    pub fn new(config: ExplorationPolicyConfig) -> Result<Self, PolicyError> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &ExplorationPolicyConfig {
        &self.config
    }

    fn priority(&self, candidate: &PolicyCandidate) -> f64 {
        candidate.score.utility
            - self.config.depth_penalty * f64::from(candidate.depth)
            - self.config.node_cost_penalty * candidate.cost_units as f64
    }
}

impl ExplorationPolicy for LinearExplorationPolicy {
    fn select(&self, view: &PolicyView) -> PolicyAction {
        if view.remaining_cost_units == 0
            || view.round >= self.config.max_rounds
            || view.stagnation_rounds >= self.config.patience_rounds
            || view.candidates.is_empty()
        {
            return PolicyAction::stop();
        }

        let mut ranked = view.candidates.clone();
        ranked.sort_by(|a, b| {
            b.score
                .feasible
                .cmp(&a.score.feasible)
                .then_with(|| self.priority(b).total_cmp(&self.priority(a)))
                .then_with(|| a.created_seq.cmp(&b.created_seq))
                .then_with(|| a.node_id.cmp(&b.node_id))
        });

        let mut expand = Vec::with_capacity(self.config.worker_limit);
        for candidate in ranked {
            for _ in 0..self.config.fanout_per_parent {
                if expand.len() == self.config.worker_limit {
                    break;
                }
                expand.push(candidate.node_id);
            }
            if expand.len() == self.config.worker_limit {
                break;
            }
        }

        PolicyAction {
            stop: expand.is_empty(),
            expand,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("invalid exploration policy: {0}")]
    Invalid(&'static str),
}
