# pg-tsy-core

A compact, contract-first quantitative trading monorepo designed for **one engineer + AI agents**.

It follows three independent but composable technical paths:

1. **Python factor research** — Parquet/S3 → Arrow/Polars → factor discovery, evaluation and backtests.
2. **Local ML / LOB research** — PyTorch/JAX-oriented datasets, walk-forward validation and parameter optimization on a local GPU when available.
3. **Rust live trading core** — tick market data, online factors, strategy/position state machines, risk, OMS, execution, reconciliation, journal/recovery and replay.

Venue boundaries currently cover **Binance Portfolio Margin**, **Hyperliquid** and **Interactive Brokers TWS/IB Gateway**. Hyperliquid and IBKR have executable Rust adapters with explicit ambiguous-submit recovery semantics. Telegram is the operator relay/control surface.

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

 Trades/BBO/L2/Candles
          |
          v
 RollingFactorEngine
          |
          v
       Signal
          |
          v
 StrategyMachine -> PositionTarget -> Risk -> OMS -> ExecutionAdapter
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
rust/
  crates/
    pg-types/                Shared domain types
    pg-marketdata/           Trade/BBO/L2/candle + freshness/aggregation
    pg-strategy/             Online factors + entry/exit/position state machine
    pg-risk/                 Pre-trade risk engine
    pg-oms/                  Order state machine and partial-fill accounting
    pg-execution/            Venue-neutral execution/recovery contract
    pg-reconcile/            Venue ↔ journal ↔ internal state reconciliation
    pg-journal/              Durable event journal primitives
    pg-store/                PostgreSQL lease/fencing/order/ownership state
    pg-replay/               Deterministic event replay
    pg-control/              Telegram/operator command contract
    pg-core/                 Live/shadow orchestration binary
  adapters/
    pg-binance/              Binance/Portfolio Margin boundary
    pg-hyperliquid/          Official Hyperliquid Rust SDK boundary
    pg-ibkr/                 Community Rust IBKR/TWS boundary
docs/                       Architecture, ADRs, runbooks and status
infra/                      AWS/Terraform deployment blueprint
```

## Strategy automation

The live Rust path computes lightweight online factors from subscribed market events. Training is not required on the production host.

Current online factor primitives include:

- VWAP and VWAP deviation;
- trade imbalance;
- bid/ask spread;
- L2 book imbalance;
- short-horizon momentum;
- realized volatility.

`AutomationConfig` declares the required feeds, `RollingFactorEngine` maintains rolling state, and `AutomatedStrategy` applies warmup, spread/volatility gates, confidence, TTL and throttling before producing a versioned signal. Partial fills update strategy-owned position quantity incrementally; an order is never assumed fully filled merely because submission succeeded.

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

## Market-data identity rule

Never fabricate sequencing information. If a venue feed exposes a trustworthy monotonic sequence it may be used for gap detection. If it does not, `sequence` remains `None` and freshness/reconnect/snapshot logic carries the safety burden. IBKR tick-by-tick data currently follows this rule.

## Safety invariants

- Live trading is **off by default**.
- Research/training code never submits exchange orders.
- AWS live deployments do not require or install PyTorch/Optuna.
- Every strategy-created order has explicit ownership identity.
- Manual positions are never silently adopted by a strategy.
- Unknown exchange/order state is fail-closed for **new strategy exposure**.
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

## AWS production shape

- **S3**: market data/model artifacts.
- **Dedicated EC2**: Rust live core.
- **RDS PostgreSQL**: lease/fencing, journal metadata, order state, ownership, reconciliation and checkpoints.
- **Secrets Manager**: venue and Telegram credentials.
- **CloudWatch + Prometheus/Grafana**: target observability stack.
- **EC2/AWS Batch only if needed** for research jobs; local training is the default.

Do not introduce EKS/Kafka until measurements justify them.

## Current maturity

The repository has moved beyond a scaffold into a **P0 production slice / execution-and-recovery hardening stage**. Hyperliquid and IBKR execution adapters now implement stable client identity and ambiguous-submit reconciliation, and the core has OMS partial fills, ownership/reconcile primitives, PostgreSQL lease/fencing/checkpoint state and automated online factors.

It is **not yet an unattended-production release**. Remaining P0 work includes wiring the continuous reconciliation loop through the live runtime, proving durable journal-before-dispatch ordering end-to-end, completing Telegram emergency flatten, health/readiness/metrics, hardened Docker/systemd EC2 deployment, Binance PM execution/recovery and kill-9/network/database failure injection before a small-capital canary.

See [`docs/STATUS.md`](docs/STATUS.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/STORAGE_RECOVERY.md`](docs/STORAGE_RECOVERY.md), [`docs/EXCHANGES.md`](docs/EXCHANGES.md), [`docs/STRATEGY_AUTOMATION.md`](docs/STRATEGY_AUTOMATION.md) and [`docs/ROADMAP.md`](docs/ROADMAP.md).
