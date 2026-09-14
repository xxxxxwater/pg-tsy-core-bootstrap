# pg-tsy-core

A compact, contract-first quantitative trading monorepo designed for **one engineer + AI agents**.

It follows three independent but composable technical paths:

1. **Python factor research** — Parquet/S3 → Arrow/Polars → factor discovery, evaluation and backtests.
2. **Local ML / LOB research** — PyTorch/JAX-oriented datasets, walk-forward validation and parameter optimization on a local GPU when available.
3. **Rust live trading core** — tick market data, online factors, portable strategy policies, position state machines, risk, OMS, execution, reconciliation, journal/recovery and replay.

Venue boundaries currently cover **Binance Portfolio Margin**, **Hyperliquid** and **Interactive Brokers TWS/IB Gateway**. Hyperliquid and IBKR have executable Rust adapters with explicit ambiguous-submit recovery semantics; neither adapter is constructed by the shipping daemon yet. IBKR is additionally a runtime **market-data** source. Telegram is the operator relay/control surface.

> `pg-tsy-core` is an independent PG project inspired by common modern quant architecture patterns. It is not TSY Capital source code and is not affiliated with TSY Capital.

## Design rule

**Research proposes. Rust disposes.**

Python can produce a signal, model artifact, parameter set or feature definition. Only the Rust live core may turn that into order intent after market-data freshness, strategy state, position ownership, reconciliation and risk checks.

```text
                    LOCAL / RESEARCH

 S3/Parquet -> Arrow/Polars -> Factors --------+
                                                 |
 L2/trades -> PyTorch/JAX -> ML signal ---------+--> signal.v1
       ^           |
       |           +--> walk-forward / robustness / Optuna
       |                    (local only)
       |
 websocket/TWS tick capture

                         |
                         v

                    AWS / LIVE RUST

                  strategy.v1
                       |
                       v
                 Rule Graph
                       |
                required_features
                       |
                       v
             FeatureProviderRegistry
                       |
                minimal FeedSpec set
                       |
                       v
 Binance / Hyperliquid / IBKR adapters
                       |
             Trades / BBO / L2 / Candles
                       |
                       v
               LiveFeatureEngine
                       |
                 FeatureFrame
                       |
             portable PolicyEngine
                       |
              Signal / PositionTarget
                       |
           Risk -> OMS -> ExecutionAdapter
                         /          \
                 Hyperliquid       IBKR
                     |              |
               cloid identity   order_ref identity
                     \              /
                      venue truth
                          |
                Ack / Fill / Reject
                          |
            Journal / Reconcile / Recovery
                          |
            Position ownership / checkpoint
                          |
               Telegram control relay
```

The diagram is the target architecture. The daemon that ships today registers only
the in-process shadow venue as an execution adapter; see
[Shadow runtime](#shadow-runtime-current-execution-path).

## Repository map

```text
contracts/                  Versioned cross-language contracts
research/                   Python research control plane
  src/pg_tsy/
    data/                    Parquet / Arrow / Polars data access
    factor/                  Factor definitions and registry
    ml/                      Local model training + model artifact contracts
    tuning/                  Walk-forward / robust hyperparameter search
    signal/                  Signal creation and development store
strategies/                  Editable strategy.v1 definitions
rust/
  crates/
    pg-types/                Shared domain types
    pg-marketdata/           Trade/BBO/L2/candle + freshness/aggregation
    pg-strategy/             Feature providers + portable policy + state machine
    pg-risk/                 Pre-trade risk engine
    pg-oms/                  Order state machine and partial-fill accounting
    pg-execution/            Venue-neutral execution/recovery contract
    pg-reconcile/            Venue ↔ journal ↔ internal state reconciliation
    pg-journal/              Durable event journal primitives
    pg-store/                PostgreSQL lease/fencing/order/ownership state
    pg-replay/               Deterministic event replay
    pg-control/              Telegram/operator command contract
    pg-orchestrator/         Durable execution dispatch + recovery/reconcile cycles
    pg-core/                 Live/shadow orchestration binary
  adapters/
    pg-binance/              Binance/Portfolio Margin boundary
    pg-hyperliquid/          Official Hyperliquid Rust SDK boundary
    pg-ibkr/                 Community Rust IBKR/TWS boundary
docs/                       Architecture, ADRs, runbooks and status
infra/                      AWS/Terraform deployment blueprint
```

## Portable strategy infrastructure

Strategies are definitions and policies, not venue adapters. The same strategy template can expand into independent instruments such as:

```text
BINANCE_PM:ETHUSDT
HYPERLIQUID:HYPE
IBKR:AAPL
```

A rule graph references normalized feature names. `PolicyDefinition::required_features()` collects them, `FeatureProviderRegistry` verifies that each live feature has a provider, and `FeaturePlan` derives the minimal market-data subscriptions required for each instrument.

Current standard mappings include:

- Trades → `last_price`, `vwap`, `vwap_deviation_bps`, `trade_imbalance`;
- BBO → `spread_bps`;
- L2 → `book_imbalance`;
- Candle → `momentum_bps`, `realized_volatility_bps`, `volume_ratio`;
- normalized `PositionView` → quantity, average entry, filled-entry count, unrealized/peak return.

An unregistered live feature fails strategy loading instead of silently remaining missing. Custom strategy factors therefore become explicit providers with a stable feature name and declared normalized feed dependencies.

The live and fixture paths converge on the same object:

```text
fixture JSON --------------------------+
                                      |
                                      v
                                FeatureFrame
                                      |
                                      v
                                 PolicyEngine
                                      ^
                                      |
MarketEvent -> LiveFeatureEngine ------+
```

This allows migrated strategies to compare fixture decisions against live-style normalized market-event replay without importing Binance, Hyperliquid or IBKR SDK types into strategy code.

See [`docs/STRATEGIES.md`](docs/STRATEGIES.md).

## Strategy automation compatibility path

The existing Rust automation path remains supported while production orchestration moves toward portable policy-driven decisions. It computes lightweight online factors from subscribed market events and does not require training on the production host.

Current online factor primitives include:

- VWAP and VWAP deviation;
- trade imbalance;
- bid/ask spread;
- L2 book imbalance;
- short-horizon momentum;
- realized volatility.

`RollingFactorEngine` maintains rolling state, and `AutomatedStrategy` applies warmup, spread/volatility gates, confidence, TTL and throttling before producing a versioned signal. Partial fills update strategy-owned position quantity incrementally; an order is never assumed fully filled merely because submission succeeded.

## Execution idempotency and ambiguous-submit recovery

The core rule is:

> **An unknown external outcome is not permission to send a replacement order.**

A persisted order intent has stable venue identity and is reconciled before any replayed submission.

### Hyperliquid

- the persisted intent UUID is used as the venue `cloid`;
- submit checks existing venue state by `cloid` before posting;
- transport/protocol ambiguity after posting triggers another `cloid` lookup;
- if the order is found, the local runtime adopts venue truth;
- if the outcome still cannot be proven, the adapter returns `ExecutionError::Unknown` and the caller must reconcile instead of blindly posting again.

### Interactive Brokers

- `OrderIntent.client_order_id()` is written to TWS `order_ref`;
- replay checks `open_orders`, then `completed_orders`, then execution reports carrying `order_reference`;
- a fast market fill therefore remains discoverable even when it is no longer present in open orders;
- unresolved placement/cancel outcomes become `ExecutionError::Unknown` and block automatic replacement submission.

IBKR ordinary stock orders do **not** expose a crypto-style atomic reduce-only flag through this adapter. Reduce-only is disabled by default. When explicitly enabled, a software guard refreshes account position immediately before placement and rejects wrong-direction or cross-through-flat quantities. This is a software safety check, not a venue-native guarantee.

## Shadow runtime (current execution path)

`pg-core --serve` accepts `PG_RUN_MODE=shadow` only; `paper` and `live` are refused at
startup, and the process refuses to start if real-venue routing is enabled. Every
strategy decision still travels the durable production path:

```text
strategy decision
      |
pg_risk::evaluate_order          (new exposure never bypasses risk)
      |
DurableExecution::dispatch       (OrderRecord + intent journaled before the adapter call)
      |
AdapterRegistry
      |
ShadowExecutionAdapter           (in-process simulated venue, no network side effect)
      |
ack / reject / Unknown -> durable OrderRecord
```

- `PG_SHADOW_FILL_MODE` selects what the simulated venue does: `rest` (default) only
  acknowledges an order, `immediate` fills it on acknowledgement. Neither mode sends a
  real order.
- The shadow venue is registered for Hyperliquid, IBKR and Binance PM, so any
  definition whose market-data feeds the build can serve runs end to end without a
  venue credential.
- The simulated book is marked from normalized market events, and a real
  `pg_strategy::policy::PositionView` (net quantity, average entry, filled entries,
  unrealized return, peak return) is rebuilt from it on every event. It is no longer a
  constant flat view.
- Reduce-only shadow orders are rejected unless they strictly shrink an existing
  opposite-signed simulated position.
- A definition that declares a `[[policy.*]]` rule graph is owned by the portable
  policy path; the legacy score machine's `Submit` decisions for that definition are
  suppressed so one instrument never has two decision engines dispatching.
- The durable `OrderRecord` records the submit-time state. Applying simulated venue
  fills back into it is continuous reconciliation, which is not wired into the daemon
  yet.

## Market-data identity rule

Never fabricate sequencing information. If a venue feed exposes a trustworthy monotonic sequence it may be used for gap detection. If it does not, `sequence` remains `None` and freshness/reconnect/snapshot logic carries the safety burden. IBKR tick-by-tick data currently follows this rule.

## Runtime market-data sources

The daemon subscribes only to venues the build actually wires up, and a derived
subscription it cannot serve fails startup instead of reconnecting forever:

| Venue | Cargo feature | Notes |
| --- | --- | --- |
| Hyperliquid | `hyperliquid-marketdata` (default) | public websocket trades/BBO/L2/candles |
| Interactive Brokers | `ibkr-marketdata` | TWS / IB Gateway; enabled in the shipped image |
| Binance Portfolio Margin | — | no runtime market-data source yet |

```bash
# Build with both runtime market-data sources
cd rust && cargo build -p pg-core --features ibkr-marketdata
```

IBKR is a **market-data source only**. There is no IBKR execution path in the runtime,
live mode remains refused, and all execution goes to the in-process shadow venue. The
binding is the community `ibapi` crate, not an official IBKR Rust SDK. See
[`docs/EXCHANGES.md`](docs/EXCHANGES.md).

## Safety invariants

- Live trading is **off by default**.
- The shadow daemon refuses to start when real-venue routing is enabled and registers only the in-process simulated venue.
- An order intent is journaled before the execution adapter is called.
- Research/training code never submits exchange orders.
- AWS live deployments do not require or install PyTorch/Optuna.
- Strategy/policy code never imports venue SDK types.
- Rule-graph features without a registered live provider fail at load time.
- Every strategy-created order has explicit ownership identity.
- Manual positions are never silently adopted by a strategy.
- Unknown exchange/order state is fail-closed for **new strategy exposure**.
- Entry filters do not disable exit evaluation for already-owned exposure.
- Exit intents are explicitly `ReduceOnly` at the core contract; venue adapters must state whether that guarantee is native or software-enforced.
- Restart/recovery reconciles venue truth before opening new exposure.
- Signals expire and cannot be reused indefinitely.
- Telegram is an authenticated relay; it cannot bypass Risk/OMS/reconciliation.
- Telegram script commands address an allowlist; no arbitrary shell execution exists.
- A network timeout after submit is never treated as proof of rejection.

## Local research bootstrap

```bash
cd research
python -m venv .venv
source .venv/bin/activate
pip install -e '.[dev,train]'
pytest
python -m pg_tsy.cli demo-signal --asset SOLUSDT
python scripts/train_local.py --help
```

The local trainer selects CUDA, Apple MPS or CPU at runtime. Model artifacts and frozen parameter sets are exported for deployment; the live host does not train models or run hyperparameter search.

## Strategy validation and replay

```bash
# Parse definitions, compile policy graphs and validate provider/subscription plans
make strategy-validate

# Replay normalized MarketEvent JSONL through live feature generation + policy evaluation
make strategy-replay EVENTS=data/replay/hype.jsonl

# Replay fixture FeatureFrame JSONL directly through the same PolicyEngine
make policy-replay FEATURES=data/replay/policy_features.jsonl

# Replay the shipped fixtures with the fixture strategy definition
make strategy-replay EVENTS=data/replay/hype.jsonl STRATEGY_DIR=../data/replay/strategies
make policy-replay FEATURES=data/replay/policy_features.jsonl STRATEGY_DIR=../data/replay/strategies
```

`STRATEGY_DIR` defaults to `../strategies`. Replay never intentionally routes live
orders. The fixtures and both JSONL formats are documented in
[`data/replay/README.md`](data/replay/README.md).

## Rust / CI

```bash
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# External venue SDK bindings and execution/recovery tests
cargo test -p pg-hyperliquid --features sdk
cargo test -p pg-ibkr --features sdk
cargo clippy -p pg-hyperliquid -p pg-ibkr --all-targets --features sdk -- -D warnings

# Operator relay
cargo check -p pg-control --features telegram
```

These tests do not intentionally route live orders. Real-account integration/failure-injection tests remain explicit opt-in operations.

## Local development without a local Rust toolchain

The pinned toolchain also runs from a container, which keeps local results identical to
CI and to the release image:

```powershell
# One-time: seed the base images used by Dockerfile and docker-compose.yml
pwsh ./scripts/pull-base-images.ps1

# Run cargo against rust/ inside rust:1.98.1-bookworm
pwsh ./scripts/rust-docker.ps1 "fmt --check"
pwsh ./scripts/rust-docker.ps1 "clippy --workspace --all-targets -- -D warnings"
pwsh ./scripts/rust-docker.ps1 "test --workspace"

# Or all three at once
make rust-docker
```

`scripts/rust-docker.ps1` mounts the whole repository at `/src` and keeps the cargo
target directory in the `pgtsy-cargo-target` volume, so incremental builds survive
between runs and the Makefile's relative paths (`../strategies`, `../data/replay`)
resolve exactly as they do on the host. `-Image` and `-TargetVolume` override the
defaults. `scripts/pull-base-images.ps1` pulls `rust:1.98.1-bookworm`,
`debian:bookworm-slim` and `postgres:17` through a registry mirror and re-tags them
under their upstream names; it is only needed on networks where the Docker daemon
cannot reach Docker Hub.

## Operator commands

The Telegram menu maps to a small typed command protocol:

- `/start` — request strategy start after startup gates pass;
- `/performance` — performance/PnL summary;
- `/status` — runtime, venue, reconcile and strategy status;
- `/logs [n]` — bounded recent log tail;
- `/emergency_exit` — emergency flatten + halt request through the normal risk/execution path;
- `/scripts` — list allowlisted strategies/scripts;
- `/reload_script <name>` — reload one allowlisted strategy definition;
- `/latency` — market-data/order path latency statistics.

The command contract exists, but the production emergency-flatten orchestration is still part of the remaining P0 work; Telegram is not a direct exchange backdoor.

The daemon also exposes a local health/control listener (`PG_HEALTH_ADDR`, default
`0.0.0.0:8080`):

```bash
make health    # GET /healthz
make ready     # GET /readyz
make metrics   # GET /metrics (Prometheus text format)
make reload    # POST /admin/reload - re-validate and swap strategy definitions
```

It is an operator surface, not a trading surface: it can only ask the runtime to reload
strategy definitions, and it can never submit, cancel or flatten anything directly.

## AWS production shape

- **S3**: market data/model artifacts.
- **Dedicated EC2**: Rust live core.
- **RDS PostgreSQL**: lease/fencing, journal metadata, order state, ownership, reconciliation and checkpoints.
- **Secrets Manager**: venue and Telegram credentials.
- **CloudWatch + Prometheus/Grafana**: target observability stack.
- **EC2/AWS Batch only if needed** for research jobs; local training is the default.

Do not introduce EKS/Kafka until measurements justify them.

## Current maturity

The repository has moved beyond a scaffold into a **P0 production slice / execution-and-recovery hardening stage**. Hyperliquid and IBKR execution adapters implement stable client identity and ambiguous-submit reconciliation. The core also has OMS partial fills, ownership/reconcile primitives, PostgreSQL lease/fencing/checkpoint state, portable multi-venue strategy definitions, graph-derived feature subscriptions and live normalized FeatureFrame generation.

The shadow daemon now runs the whole path end to end: derived subscriptions, live
features, both strategy engines, one decided dispatcher per definition, risk, journal
before dispatch, the simulated venue, position feedback into the policy graph, real
startup-gate evaluation, `/healthz`/`/readyz`/`/metrics`, an explicit shutdown policy
and operator-triggered strategy reload.

It is **not yet an unattended-production release**. Remaining P0 work includes
continuous reconciliation that writes venue fills back into the durable order records,
end-to-end crash-window proof around every exposure-changing submit, Telegram emergency
flatten, hardened Docker/systemd EC2 deployment, Binance PM execution/recovery and
kill-9/network/database failure injection before a small-capital canary.

See [`docs/STATUS.md`](docs/STATUS.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/STORAGE_RECOVERY.md`](docs/STORAGE_RECOVERY.md), [`docs/EXCHANGES.md`](docs/EXCHANGES.md), [`docs/STRATEGIES.md`](docs/STRATEGIES.md), [`docs/STRATEGY_AUTOMATION.md`](docs/STRATEGY_AUTOMATION.md) and [`docs/ROADMAP.md`](docs/ROADMAP.md).
