# Dream-RSI in the PG-TSY Rust control plane

Date: 2026-09-27

The upstream Dream-RSI repository describes accumulated discovery history as a replay simulator over
the realized search space. Alternative exploration policies can revisit recorded branches in different
orders, with different parallel groupings and stopping decisions, while reusing already-recorded
outcomes instead of rerunning the discovery agent or evaluator. The official repository currently says
the full codebase and reproduction scripts are still being prepared, so this implementation follows
the published method contract rather than copying upstream source.

## Architecture mapping

| Dream-RSI concept | PG-TSY Rust component |
| --- | --- |
| ExplorationPolicy | `ExplorationPolicyConfig` + `LinearExplorationPolicy` |
| Strategy variants | `StrategyVariant` |
| ExperimentStore | `JsonlExperimentStore` / `MemoryExperimentStore` |
| Discovery Tree | `DiscoveryTree` |
| Backtest / isolated execution | `ExperimentExecutor` + `OnlineExplorer` |
| Evaluator | `Evaluator` |
| Replay / Dream | `ReplayEngine` + `DreamEngine` |
| New ExplorationPolicy | `DreamResult::selected` |

## Loop

```text
Dream-RSI
    |
    +-------------------+
    |                   |
    v                   v
ExplorationPolicy   ExperimentStore
    |                   |
    v                   v
Strategy variants   Discovery Tree
    |                   |
    +----> Backtest <----+
              |
              v
          Evaluator
              |
              v
         Replay / Dream
              |
              v
     New ExplorationPolicy
```

1. The current policy controls branching, worker parallelism, and stopping for isolated discovery.
2. Every realized variant and outcome is written into a `DiscoveryTree`.
3. Completed trees are persisted and form a reusable `WorldPool`.
4. `ReplayEngine` evaluates counterfactual traversal over outcomes that already exist in those trees.
5. `DreamEngine` searches local revisions of the exploration policy across the full world pool.
6. The incumbent is always a candidate, so selection cannot regress the configured objective on that
   frozen history.
7. The selected policy is used for the next isolated discovery cycle, adding new trees to the pool.

## Trading-system boundary

`pg-dream` is research/control-plane code. It has no exchange adapter, OMS, risk bypass, or credential
path. Implementations of `ExperimentExecutor` may call historical backtests, `pg-sim`, or an isolated
shadow environment, but must not call live exchange order endpoints.

Replay is limited to the realized search space. It may choose different recorded branches, order,
parallel grouping, and stopping points, but it cannot invent the outcome of a strategy variant that was
never executed and stored.

The non-regression rule applies only to the configured replay objective over a frozen `WorldPool`.
It is not evidence that the next market regime will preserve PnL, Sharpe, drawdown, latency, or fills.

## Evaluator

`Evaluator` supports hard feasibility constraints including minimum Sharpe, maximum drawdown, and
minimum trade count. `ScoreWeights` produces deterministic utility from return, Sharpe, drawdown, and
execution cost. `ReplayConfig` separately penalizes discovery cost and can reward more useful work per
decision round.


## Source evidence

The first integrated Dream-RSI source line was added on post-RC1 `main`; the stabilized source at
`3e80eaf87b4821f60af45c522ff1801147e3e89d` passed the repository's full
[CI run](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/36281063583) and
[PostgreSQL fenced-ledger run](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/36281063610).
The CI evidence includes workspace `cargo fmt --check`, workspace-wide
`cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace`.

That evidence means the crate integrates with the current source tree and tests. It does not prove
that an unobserved strategy variant would have the replayed score, that a future market regime will
match historical results, or that any paper/live venue is accepted.

## Integration contract for higher-level agents

A higher-level control plane such as PureGamma.ai should translate natural-language research intent
into a validated strategy search space plus deterministic evaluator constraints. For example,
“BTC 15m, maximum drawdown below 12%, Sharpe above 1.8” maps naturally to a BTC/15m variant generator
plus `EvaluationConstraints { min_sharpe: Some(1.8), max_drawdown_pct: Some(12.0), ... }`.

The higher-level agent may propose variants and consume Dream results, but Rust remains authoritative
for the recorded world, experiment lineage, deterministic score, replay trace and selected exploration
configuration. Any promotion toward trading remains a separate reviewed step through the existing
strategy/risk/OMS/execution/reconcile chain.
