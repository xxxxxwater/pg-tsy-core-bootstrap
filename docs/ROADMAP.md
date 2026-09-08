# Roadmap

The roadmap is organized around **operational invariants**, not crate count. The repository has moved beyond the original scaffold and is currently in the **P0 production slice / execution-and-recovery hardening** stage.

## Completed foundation

- [x] Versioned signal contract.
- [x] Python research package boundaries.
- [x] Local ML/tuning profile separated from the AWS live runtime.
- [x] Rust domain crates for market data, strategy, risk, OMS, execution, reconcile, journal, store and replay.
- [x] Live trading disabled by default with shadow/paper/live runtime modes.
- [x] PostgreSQL lease/fencing/checkpoint/order/ownership primitives.
- [x] Strategy/manual/unknown ownership model.
- [x] Partial-fill-aware OMS and strategy position state.
- [x] CI for core Rust, Python and optional venue/control integrations.

## Completed strategy/data slice

- [x] Common Trade/BBO/L2/Candle event model.
- [x] Feed freshness and gap primitives.
- [x] Online Rust factors: VWAP deviation, trade imbalance, spread, L2 imbalance, momentum and realized volatility.
- [x] Automated subscription → factor → signal → StrategyMachine path.
- [x] Signal TTL, confidence, warmup, volatility/spread gates and throttling.
- [x] Local research/training path with walk-forward/robustness orientation.

Still to deepen later:

- immutable large-scale dataset manifests and production collectors;
- complete factor IC/correlation/regime/turnover tooling;
- large L2/L3 retention and replay datasets;
- stronger model registry/artifact promotion workflow.

## Completed P0 venue execution slice

### Hyperliquid

- [x] Official Rust SDK pinned to a reviewed commit.
- [x] Trades/BBO/L2/candle websocket mapping.
- [x] ExecutionAdapter submit/cancel/read-side state.
- [x] Persisted intent UUID mapped to `cloid`.
- [x] Pre-submit duplicate lookup.
- [x] Ambiguous submit recovery by stable `cloid`.
- [x] Partial-fill state mapping.
- [x] Feature tests and Clippy in CI.

### Interactive Brokers

- [x] Community `ibapi 4.0.1` isolated behind adapter boundary.
- [x] TWS/IB Gateway tick-by-tick trades and BBO.
- [x] Market depth and realtime bars.
- [x] ExecutionAdapter submit/cancel/read-side state.
- [x] Stable `order_ref` identity.
- [x] Recovery through open orders → completed orders → execution reports.
- [x] Ambiguous placement/cancel fail-closed behavior.
- [x] Optional software reduce-only guard with cross-through-flat rejection.
- [x] Feature tests and Clippy in CI.

## Current milestone — end-to-end recovery proof

The venue adapters can now recognize previously accepted orders after ambiguous outcomes. The next milestone is to connect that to the complete live runtime and prove the invariant under crashes.

### P0.1 Continuous reconcile

- [ ] Run venue snapshots continuously, not only through adapter methods/tests.
- [ ] Reconcile open orders, fills, positions and ownership against durable state.
- [ ] Persist reconcile reports and affected venue+asset SAFE_HOLD scopes.
- [ ] Define recovery cadence/backoff and stale-reconcile gates.

### P0.2 Journal-before-dispatch

- [ ] Persist order intent and dispatch state before entering a path where the venue may accept the request.
- [ ] Bind every durable mutation to the current fencing token.
- [ ] Ensure a restarted process never interprets "ACK not persisted" as "order not accepted".
- [ ] Add explicit recovery state for dispatch-started / outcome-unknown.

### P0.3 Failure injection

Required scenarios include:

- [ ] kill-9 immediately before submit;
- [ ] kill-9 after venue acceptance but before ACK persistence;
- [ ] network timeout after venue acceptance;
- [ ] venue lookup temporarily unable to find a just-accepted order;
- [ ] PostgreSQL unavailable before/after dispatch;
- [ ] old fenced instance reconnecting after a new leader takes over;
- [ ] partial fill followed by restart;
- [ ] cancel ambiguity followed by fill;
- [ ] manual position coexisting with strategy-owned position state.

Exit criterion:

> Repeated fault injection demonstrates that restart/reconcile reconstructs venue truth and does not emit blind duplicate exposure-increasing orders.

## P0.4 Operator emergency path

- [ ] Wire Telegram `/emergency_exit` to authenticated command audit.
- [ ] Convert the command into idempotent target-flat/reduce requests.
- [ ] Route emergency actions through ownership → Risk → OMS → Execution, never directly to a venue SDK.
- [ ] HALT new exposure while allowing safe flatten/reconcile behavior.
- [ ] Define behavior when emergency flatten itself has an ambiguous outcome.

## P0.5 Observability / operations

- [ ] `/healthz` for process liveness.
- [ ] `/readyz` for database/lease/feed/reconcile/trading readiness.
- [ ] Prometheus metrics for market-data age, gaps, order latency, unknown outcomes, reconcile mismatches, fencing and command activity.
- [ ] Alert rules for stale feed, lease loss, Unknown order, ownership mismatch and reconciliation lag.
- [ ] Hardened production Docker image.
- [ ] systemd unit/restart policy for EC2.
- [ ] graceful shutdown and startup-gate runbook.

## P0.6 Binance Portfolio Margin parity

- [ ] Production PM market/account/user-data integration.
- [ ] Stable client-order identity and ambiguous-submit recovery.
- [ ] PM-specific risk/balance/position reconciliation.
- [ ] Reduce-only/stop/emergency semantics.
- [ ] Kill-9/network/database failure-injection acceptance.

Binance PM is not considered production-complete merely because an adapter boundary exists.

## Canary progression

Only after the P0 recovery/operations criteria are demonstrated:

```text
historical/replay
      |
shadow
      |
paper
      |
tiny allowlisted canary
      |
limited unattended live
```

Each transition requires a written acceptance report; no stage is skipped because unit tests are green.

## Later research expansion

### LOB / ML

- DeepLOB baseline before more complex architectures.
- TLOB/transformer benchmarks.
- Cost-aware scoring: expected edge - fee - spread - slippage - adverse selection.
- hftbacktest-style queue/latency replay.
- JAX-LOB/AlphaTrade execution-policy experiments.
- optional ONNX/Rust inference only where latency measurements justify it.

### Scaling only when measured

Possible later additions:

- gRPC or shared memory between independently scaling processes;
- larger distributed research compute;
- dedicated capture/replay nodes;
- multi-account/multi-strategy scheduling.

## Explicit non-goals until needed

- Kubernetes/EKS by default.
- Kafka cluster by default.
- dozens of microservices.
- active-active multi-region live trading.
- online production model training/hyperparameter search.
- RL deciding portfolio direction before strong statistical/supervised baselines and execution-cost validation exist.
