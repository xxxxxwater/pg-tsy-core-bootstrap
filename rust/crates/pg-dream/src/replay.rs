use crate::{
    DiscoveryTree, EvaluationError, Evaluator, ExplorationPolicy, ExplorationPolicyConfig,
    LinearExplorationPolicy, NodeId, NodeScore, PolicyCandidate, PolicyError, PolicyView,
    WorldError, WorldPool,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayConfig {
    pub max_cost_units: u64,
    pub max_rounds: u32,
    pub discovery_cost_penalty: f64,
    pub parallelism_bonus: f64,
    pub infeasible_penalty: f64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            max_cost_units: 1_000,
            max_rounds: 64,
            discovery_cost_penalty: 0.0,
            parallelism_bonus: 0.0,
            infeasible_penalty: 1_000_000.0,
        }
    }
}

impl ReplayConfig {
    fn validate(&self) -> Result<(), ReplayError> {
        if self.max_cost_units == 0 {
            return Err(ReplayError::InvalidConfig(
                "max_cost_units must be positive".into(),
            ));
        }
        if self.max_rounds == 0 {
            return Err(ReplayError::InvalidConfig(
                "max_rounds must be positive".into(),
            ));
        }

        for (name, value) in [
            ("discovery_cost_penalty", self.discovery_cost_penalty),
            ("parallelism_bonus", self.parallelism_bonus),
            ("infeasible_penalty", self.infeasible_penalty),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(ReplayError::InvalidConfig(format!(
                    "{name} must be finite and non-negative"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayStep {
    pub round: u32,
    pub selected_parents: Vec<NodeId>,
    pub revealed_nodes: Vec<NodeId>,
    pub best_node_id: NodeId,
    pub best_utility: f64,
    pub cost_units: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub tree_id: uuid::Uuid,
    pub best_node_id: NodeId,
    pub best_score: NodeScore,
    pub revealed_nodes: usize,
    pub decision_rounds: u32,
    pub cost_units: u64,
    pub history_exhausted: bool,
    pub objective: f64,
    pub trace: Vec<ReplayStep>,
}

#[derive(Debug, Clone)]
pub struct ReplayEngine {
    evaluator: Evaluator,
    config: ReplayConfig,
}

impl ReplayEngine {
    pub fn new(evaluator: Evaluator, config: ReplayConfig) -> Result<Self, ReplayError> {
        config.validate()?;
        Ok(Self { evaluator, config })
    }

    pub fn evaluator(&self) -> &Evaluator {
        &self.evaluator
    }

    pub fn config(&self) -> ReplayConfig {
        self.config
    }

    /// Replays an alternative exploration policy over outcomes that already
    /// exist in the discovery tree. This method never calls ExperimentExecutor.
    pub fn run<P>(&self, tree: &DiscoveryTree, policy: &P) -> Result<ReplayResult, ReplayError>
    where
        P: ExplorationPolicy,
    {
        tree.validate()?;

        let root = tree.root()?;
        let mut revealed = BTreeSet::from([root.node_id]);
        let mut child_cursor = BTreeMap::<NodeId, usize>::new();
        let mut best_node_id = root.node_id;
        let mut best_score = self.evaluator.score(&root.outcome)?;
        let mut cost_units = root.outcome.cost_units;
        let mut round = 0_u32;
        let mut stagnation_rounds = 0_u32;
        let mut trace = Vec::new();

        loop {
            if round >= self.config.max_rounds || cost_units >= self.config.max_cost_units {
                break;
            }

            let candidates = self.policy_candidates(tree, &revealed, &child_cursor)?;
            if candidates.is_empty() {
                break;
            }

            let action = policy.select(&PolicyView {
                round,
                remaining_cost_units: self.config.max_cost_units.saturating_sub(cost_units),
                stagnation_rounds,
                candidates: candidates.clone(),
            });
            if action.stop || action.expand.is_empty() {
                break;
            }

            let eligible = candidates
                .iter()
                .map(|candidate| candidate.node_id)
                .collect::<BTreeSet<_>>();
            for node_id in &action.expand {
                if !eligible.contains(node_id) {
                    return Err(ReplayError::IneligibleAction(*node_id));
                }
            }

            round = round.saturating_add(1);
            let selected_parents = action.expand.clone();
            let previous_best = best_node_id;
            let mut newly_revealed = Vec::new();
            let mut budget_hit = false;

            for parent_id in action.expand {
                let children = tree.children_of(parent_id);
                let cursor = child_cursor.entry(parent_id).or_default();
                let Some(child) = children.get(*cursor).copied() else {
                    continue;
                };

                let next_cost = cost_units.saturating_add(child.outcome.cost_units);
                if next_cost > self.config.max_cost_units {
                    budget_hit = true;
                    break;
                }
                *cursor += 1;

                if revealed.insert(child.node_id) {
                    cost_units = next_cost;
                    newly_revealed.push(child.node_id);
                    let score = self.evaluator.score(&child.outcome)?;
                    if self.evaluator.is_better(&score, &best_score) {
                        best_node_id = child.node_id;
                        best_score = score;
                    }
                }
            }

            if best_node_id == previous_best {
                stagnation_rounds = stagnation_rounds.saturating_add(1);
            } else {
                stagnation_rounds = 0;
            }

            trace.push(ReplayStep {
                round,
                selected_parents,
                revealed_nodes: newly_revealed,
                best_node_id,
                best_utility: best_score.utility,
                cost_units,
            });

            if budget_hit {
                break;
            }
        }

        let final_candidates = self.policy_candidates(tree, &revealed, &child_cursor)?;
        let revealed_attempts = revealed.len().saturating_sub(1) as f64;
        let attempts_per_round = if round == 0 {
            0.0
        } else {
            revealed_attempts / f64::from(round)
        };
        let mut objective = best_score.utility
            - self.config.discovery_cost_penalty * cost_units as f64
            + self.config.parallelism_bonus * attempts_per_round;
        if !best_score.feasible {
            objective -= self.config.infeasible_penalty;
        }

        Ok(ReplayResult {
            tree_id: tree.tree_id,
            best_node_id,
            best_score,
            revealed_nodes: revealed.len(),
            decision_rounds: round,
            cost_units,
            history_exhausted: final_candidates.is_empty(),
            objective,
            trace,
        })
    }

    fn policy_candidates(
        &self,
        tree: &DiscoveryTree,
        revealed: &BTreeSet<NodeId>,
        child_cursor: &BTreeMap<NodeId, usize>,
    ) -> Result<Vec<PolicyCandidate>, ReplayError> {
        let mut candidates = Vec::new();

        for node_id in revealed {
            let children = tree.children_of(*node_id);
            let cursor = child_cursor.get(node_id).copied().unwrap_or(0);
            if cursor >= children.len() {
                continue;
            }

            let node = tree.node(*node_id)?;
            candidates.push(PolicyCandidate {
                node_id: *node_id,
                depth: tree.depth(*node_id)?,
                created_seq: node.created_seq,
                cost_units: node.outcome.cost_units,
                score: self.evaluator.score(&node.outcome)?,
            });
        }

        Ok(candidates)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DreamCandidateScore {
    pub policy: ExplorationPolicyConfig,
    pub average_objective: f64,
    pub per_tree_objective: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DreamResult {
    pub incumbent: ExplorationPolicyConfig,
    pub selected: ExplorationPolicyConfig,
    pub baseline_objective: f64,
    pub selected_objective: f64,
    pub candidates: Vec<DreamCandidateScore>,
}

#[derive(Debug, Clone)]
pub struct DreamEngine {
    replay: ReplayEngine,
}

impl DreamEngine {
    pub fn new(replay: ReplayEngine) -> Self {
        Self { replay }
    }

    pub fn improve(
        &self,
        worlds: &WorldPool,
        incumbent: &ExplorationPolicyConfig,
    ) -> Result<DreamResult, ReplayError> {
        if worlds.is_empty() {
            return Err(ReplayError::EmptyWorldPool);
        }
        incumbent.validate()?;

        let mut candidates = Vec::new();
        for config in incumbent.revisions() {
            let policy = LinearExplorationPolicy::new(config.clone())?;
            let mut per_tree_objective = Vec::with_capacity(worlds.len());

            for tree in worlds.trees() {
                per_tree_objective.push(self.replay.run(tree, &policy)?.objective);
            }

            let average_objective =
                per_tree_objective.iter().sum::<f64>() / per_tree_objective.len() as f64;
            candidates.push(DreamCandidateScore {
                policy: config,
                average_objective,
                per_tree_objective,
            });
        }

        let baseline_objective = candidates
            .first()
            .ok_or(ReplayError::EmptyCandidateSet)?
            .average_objective;
        let mut selected_index = 0_usize;

        for (index, candidate) in candidates.iter().enumerate().skip(1) {
            if candidate
                .average_objective
                .total_cmp(&candidates[selected_index].average_objective)
                .is_gt()
            {
                selected_index = index;
            }
        }

        Ok(DreamResult {
            incumbent: incumbent.clone(),
            selected: candidates[selected_index].policy.clone(),
            baseline_objective,
            selected_objective: candidates[selected_index].average_objective,
            candidates,
        })
    }
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error(transparent)]
    World(#[from] WorldError),
    #[error(transparent)]
    Evaluation(#[from] EvaluationError),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error("invalid replay config: {0}")]
    InvalidConfig(String),
    #[error("exploration policy selected ineligible node {0}")]
    IneligibleAction(NodeId),
    #[error("cannot dream without at least one stored world")]
    EmptyWorldPool,
    #[error("dream candidate set is empty")]
    EmptyCandidateSet,
}
