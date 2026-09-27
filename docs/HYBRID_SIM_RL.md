# Hybrid Python/Rust simulation and training plane

Python stays a strategy/research surface for feature work, debugging, notebooks, RL/ES policy code and batched experiments. Rust owns deterministic execution semantics and now also owns the Dream-RSI exploration-control primitives in `pg-dream`: recorded worlds, isolated experiment execution contracts, deterministic scoring and historical off-policy replay.

The pg-sim crate is the reference matching kernel. The Python BatchMarketEnv is intentionally a lightweight vectorized training loop; it does not pretend to model live exchange microstructure.

Implemented Rust simulator semantics include IOC, FOK, GTC, GTD, DAY, AT_THE_OPEN, AT_THE_CLOSE, post-only, reduce-only, iceberg display quantity, OCO, OTO and OUO. OUO is defined here as one-updates-other quantity: each fill reduces the peer total quantity by the same amount.

None of these simulator capabilities automatically enables a live order type. Binance PM, Hyperliquid and IBKR adapters must advertise and validate their own capabilities. Unsupported semantics must fail closed rather than silently downgrade.

pg-core already uses Tokio. The production binary also installs mimalloc as its global allocator in this change. Performance claims must be based on benchmark evidence; no fixed interactions-per-second promise is asserted.

For RL/ES, use BatchMarketEnv to run many environments per Python call and reserve pg-sim for exchange-sensitive execution validation. A future PyO3 or Arrow shared-memory bridge can remove the remaining Python/Rust boundary once benchmark data justifies it.


## Dream-RSI layer

`pg-sim` answers **how an isolated order behaves under simulator semantics**. `pg-dream` answers **how the research process should explore recorded strategy variants**. These are separate layers:

```text
ExplorationPolicy
      |
      v
StrategyVariant -----> ExperimentExecutor -----> ExperimentOutcome
      |                        |                         |
      |                   backtest/pg-sim               v
      |                                            DiscoveryTree
      |                                                 |
      +-------------------------------------------> ExperimentStore
                                                        |
                                                        v
                                                     WorldPool
                                                        |
                                               Evaluator + Replay
                                                        |
                                                        v
                                              New ExplorationPolicy
```

The replay path never calls `ExperimentExecutor`; it consumes outcomes already stored in the discovery tree. This keeps Dream feedback cheap and prevents a replay from silently becoming a second execution engine.

The current Rust policy exposes three orchestration dimensions: branching through `fanout_per_parent`, work selected per decision round through `worker_limit`, and stopping through `patience_rounds`, `max_rounds`, and `ReplayConfig::max_cost_units`. Hard evaluator constraints include minimum Sharpe, maximum drawdown and minimum trade count, while score weights combine return, Sharpe, drawdown and execution cost into a deterministic utility.

Dream-RSI does not change the production authority chain. Any strategy promoted from research still has to pass the normal strategy -> risk -> OMS -> durable execution -> venue adapter -> reconcile path. Historical replay improvement is not live-market acceptance.
