# pg-dream

`pg-dream` is the Rust Dream-RSI exploration control plane for PG-TSY.

It owns four boundaries:

- **World**: immutable `DiscoveryTree` histories and the growing `WorldPool`.
- **Execution**: `OnlineExplorer` plus `ExperimentExecutor` for offline backtests or shadow simulation.
- **Scoring**: deterministic `Evaluator` constraints and utility.
- **Replay / Dream**: off-policy traversal of stored histories and incumbent-preserving exploration-policy improvement.

The exploration policy makes the orchestration knobs explicit:

- branching: `fanout_per_parent`
- parallelism: `worker_limit`
- stopping: `patience_rounds`, `max_rounds`, and the replay cost budget

## Safety boundary

This crate deliberately has no dependency on venue adapters, OMS, or live execution. An
`ExperimentExecutor` implementation must be offline or shadow-only. Replay reads stored outcomes and
does not rerun the experiment executor.

The incumbent policy is always included in Dream candidate evaluation, so the selected policy cannot
have a lower configured objective on the same frozen replay world pool. That is a historical replay
property, not a guarantee of future Sharpe, drawdown, PnL, or live fill quality.
