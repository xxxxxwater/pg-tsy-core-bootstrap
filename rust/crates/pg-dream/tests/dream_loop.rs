use pg_dream::{
    DreamEngine, EvaluationConstraints, Evaluator, ExperimentExecutor, ExperimentMetrics,
    ExperimentOutcome, ExperimentStore, ExplorationPolicyConfig, JsonlExperimentStore,
    LinearExplorationPolicy, OnlineExplorer, ReplayConfig, ReplayEngine, ScoreWeights,
    StrategyVariant, WorldPool,
};
use serde_json::json;
use std::error::Error;

fn outcome(
    net_return_pct: f64,
    sharpe: f64,
    max_drawdown_pct: f64,
    cost_units: u64,
) -> ExperimentOutcome {
    ExperimentOutcome {
        success: true,
        metrics: ExperimentMetrics {
            net_return_pct,
            sharpe,
            max_drawdown_pct,
            trades: 20,
            turnover: 2.0,
            execution_cost_bps: 0.0,
        },
        cost_units,
        diagnostics: json!({}),
    }
}

fn branching_world() -> pg_dream::DiscoveryTree {
    let root_variant = StrategyVariant::root("baseline", json!({"rsi": 14}));
    let mut tree =
        pg_dream::DiscoveryTree::new(root_variant.clone(), outcome(0.0, 0.0, 0.0, 1));
    let root_id = tree.root_id;

    let branch_a = StrategyVariant::child(&root_variant, "branch-a", json!({"rsi": 10}));
    let branch_a_id = tree
        .append(
            root_id,
            branch_a.clone(),
            outcome(1.0, 0.0, 0.0, 1),
        )
        .unwrap();

    let branch_b = StrategyVariant::child(&root_variant, "branch-b", json!({"rsi": 20}));
    tree.append(root_id, branch_b, outcome(2.0, 0.0, 0.0, 1))
        .unwrap();

    let branch_a_refined =
        StrategyVariant::child(&branch_a, "branch-a-refined", json!({"rsi": 8}));
    tree.append(
        branch_a_id,
        branch_a_refined,
        outcome(5.0, 0.0, 0.0, 1),
    )
    .unwrap();

    tree
}

#[test]
fn evaluator_supports_sharpe_and_drawdown_constraints() {
    let evaluator = Evaluator::new(
        EvaluationConstraints {
            min_sharpe: Some(1.8),
            max_drawdown_pct: Some(12.0),
            min_trades: Some(10),
        },
        ScoreWeights::default(),
    )
    .unwrap();

    assert!(
        evaluator
            .score(&outcome(10.0, 2.0, 11.0, 1))
            .unwrap()
            .feasible
    );
    assert!(
        !evaluator
            .score(&outcome(10.0, 1.7, 11.0, 1))
            .unwrap()
            .feasible
    );
    assert!(
        !evaluator
            .score(&outcome(10.0, 2.0, 12.1, 1))
            .unwrap()
            .feasible
    );
}

#[test]
fn dreaming_keeps_incumbent_and_can_find_better_replay_policy() {
    let worlds = WorldPool::from_trees(vec![branching_world()]).unwrap();
    let replay = ReplayEngine::new(
        Evaluator::default(),
        ReplayConfig {
            max_cost_units: 100,
            max_rounds: 8,
            discovery_cost_penalty: 0.0,
            parallelism_bonus: 0.0,
            infeasible_penalty: 1_000.0,
        },
    )
    .unwrap();
    let dream = DreamEngine::new(replay);

    let incumbent = ExplorationPolicyConfig {
        worker_limit: 2,
        fanout_per_parent: 2,
        depth_penalty: 0.0,
        node_cost_penalty: 0.0,
        patience_rounds: 2,
        max_rounds: 6,
    };
    let result = dream.improve(&worlds, &incumbent).unwrap();

    assert_eq!(result.candidates.first().unwrap().policy, incumbent);
    assert!(result.selected_objective >= result.baseline_objective);
    assert!(result.selected_objective > result.baseline_objective);
}

#[derive(Debug, Default)]
struct CountingExecutor {
    calls: usize,
}

impl ExperimentExecutor for CountingExecutor {
    fn execute(
        &mut self,
        variant: &StrategyVariant,
    ) -> Result<ExperimentOutcome, Box<dyn Error + Send + Sync + 'static>> {
        self.calls += 1;
        Ok(outcome(variant.generation as f64, 0.0, 0.0, 1))
    }
}

#[test]
fn online_execution_is_explicit_and_replay_never_calls_executor() {
    let root = StrategyVariant::root("root", json!({}));
    let child_a = StrategyVariant::child(&root, "a", json!({}));
    let child_b = StrategyVariant::child(&root, "b", json!({}));

    let mut explorer = OnlineExplorer::new(CountingExecutor::default());
    let mut tree = explorer.start(root).unwrap();
    let root_id = tree.root_id;
    explorer
        .expand(&mut tree, root_id, [child_a, child_b])
        .unwrap();
    assert_eq!(explorer.executor().calls, 3);

    let replay = ReplayEngine::new(Evaluator::default(), ReplayConfig::default()).unwrap();
    let policy = LinearExplorationPolicy::new(ExplorationPolicyConfig::default()).unwrap();
    let result = replay.run(&tree, &policy).unwrap();

    assert!(result.revealed_nodes >= 1);
    assert_eq!(explorer.executor().calls, 3);
}

#[test]
fn jsonl_store_round_trips_worlds() {
    let tree = branching_world();
    let path = std::env::temp_dir().join(format!(
        "pg-dream-{}-{}.jsonl",
        std::process::id(),
        tree.tree_id
    ));
    let mut store = JsonlExperimentStore::new(&path);
    store.append(&tree).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.trees()[0], tree);

    std::fs::remove_file(path).unwrap();
}
