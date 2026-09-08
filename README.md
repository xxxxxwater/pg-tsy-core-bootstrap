# pg-tsy-core

A compact, contract-first quantitative trading monorepo designed for **one engineer + AI agents**.

It follows three independent but composable technical paths:

1. **Python factor research** — Parquet/S3 → Arrow/Polars → factor discovery, evaluation and backtests.
2. **Local ML / LOB research** — PyTorch/JAX-oriented datasets, walk-forward validation and parameter optimization on a local GPU when available.
3. **Rust live trading core** — tick market data, strategy/position state machines, risk, OMS, execution, reconciliation, journal/recovery and replay.

Venue boundaries currently cover **Binance PM**, **Hyperliquid** and **Interactive Brokers TWS/IB Gateway**. Telegram is the operator relay/control surface.

> `pg-tsy-core` is an independent PG project inspired by common modern quant architecture patterns. It is not TSY Capital source code and is not affiliated with TSY Capital.

## Design rule

**Research proposes. Rust disposes.**

Python can produce a signal, model artifact or feature set. Only the Rust live core may turn that into order intent after strategy-state, ownership, reconciliation and risk checks.

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

 MarketData -> Strategy State -> Risk -> OMS -> Execution
     |                                         /    |     \
   Tick/L2                              Binance   HL   IBKR
     |
 Reconcile <-> Journal/Replay <-> Position Ownership
     |
 Telegram control/query relay
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
    pg-marketdata/           Tick/BBA/candle + sequencing/aggregation
    pg-strategy/             Entry/exit/position state machine
    pg-risk/                 Pre-trade risk engine
    pg-oms/                  Order state machine
    pg-execution/            Execution adapter interface
    pg-reconcile/            Venue ↔ journal ↔ internal state reconciliation
    pg-journal/              Durable event journal
    pg-replay/               Deterministic event replay
    pg-control/              Telegram/operator command contract
    pg-core/                 Live/shadow orchestration binary
  adapters/
    pg-binance/              Binance/Portfolio Margin boundary
    pg-hyperliquid/          Official Hyperliquid Rust SDK boundary
    pg-ibkr/                 Community Rust IBKR/TWS boundary
docs/                       Architecture, ADRs, runbooks
infra/                      AWS/Terraform deployment blueprint
```

## Safety invariants

- Live trading is **off by default**.
- Research/training code never submits exchange orders.
- AWS live deployments do not require or install PyTorch/Optuna.
- Every strategy-created order has explicit ownership identity.
- Manual positions are never silently adopted by a strategy.
- Unknown exchange/order state is fail-closed for **new strategy exposure**.
- Exit intents are explicitly `ReduceOnly`.
- Restart/recovery reconciles venue truth before opening new exposure.
- Signals expire and cannot be reused indefinitely.
- Telegram is an authenticated relay; it cannot bypass Risk/OMS/reconciliation.
- Telegram script commands address an allowlist; no arbitrary shell execution exists.

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

The local trainer selects CUDA, Apple MPS or CPU at runtime. Model artifacts are exported for deployment; the live host does not train models.

## Rust

```bash
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Optional external SDK bindings
cargo check -p pg-hyperliquid --features sdk
cargo check -p pg-ibkr --features sdk
cargo check -p pg-control --features telegram
```

## Operator commands

The Telegram menu maps to a small typed command protocol:

- `/start` — request strategy start after startup gates pass;
- `/performance` — performance/PnL summary;
- `/status` — runtime, venue, reconcile and strategy status;
- `/logs [n]` — bounded recent log tail;
- `/emergency_exit` — idempotent reduce-only flatten + halt request;
- `/scripts` — list allowlisted strategies/scripts;
- `/reload_script <name>` — reload one allowlisted strategy definition;
- `/latency` — market-data/order path latency statistics.

## AWS production shape

- **S3**: market data/model artifacts.
- **Dedicated EC2**: Rust live core.
- **RDS PostgreSQL**: journal metadata, ownership, checkpoints.
- **Secrets Manager**: exchange and Telegram credentials.
- **CloudWatch + Prometheus/Grafana**: observability.
- **EC2/AWS Batch only if needed** for research jobs; local training is the default.

Do not introduce EKS/Kafka until measurements justify them.

See [`docs/ROADMAP.md`](docs/ROADMAP.md), [`docs/LOCAL_TRAINING.md`](docs/LOCAL_TRAINING.md) and [`docs/CONTROL_PLANE.md`](docs/CONTROL_PLANE.md).
