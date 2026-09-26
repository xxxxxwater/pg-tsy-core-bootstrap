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
