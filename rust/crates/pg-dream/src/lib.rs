#![forbid(unsafe_code)]

//! Dream-RSI exploration control plane.
//!
//! This crate owns recorded worlds, isolated experiment execution contracts,
//! deterministic evaluation, and replay/dream policy improvement. It has no
//! venue-adapter or OMS dependency and therefore no live-order authority.

mod execution;
mod policy;
mod replay;
mod score;
mod store;
mod world;

pub use execution::{ExecutionError, ExperimentExecutor, ExploreError, OnlineExplorer};
pub use policy::{
    ExplorationPolicy, ExplorationPolicyConfig, LinearExplorationPolicy, PolicyAction,
    PolicyCandidate, PolicyError, PolicyView,
};
pub use replay::{
    DreamCandidateScore, DreamEngine, DreamResult, ReplayConfig, ReplayEngine, ReplayError,
    ReplayResult, ReplayStep,
};
pub use score::{EvaluationConstraints, EvaluationError, Evaluator, NodeScore, ScoreWeights};
pub use store::{ExperimentStore, JsonlExperimentStore, MemoryExperimentStore, StoreError};
pub use world::{
    DiscoveryNode, DiscoveryTree, ExperimentMetrics, ExperimentOutcome, NodeId, StrategyVariant,
    WorldError, WorldPool,
};
