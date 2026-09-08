# pg-tsy-core

A compact, contract-first quantitative trading monorepo designed for **one engineer + AI agents**.

It follows three independent but composable technical paths:

1. **Python factor research** — Parquet/S3 → Arrow/Polars → factor discovery, evaluation and backtests.
2. **Python ML / LOB research** — PyTorch/JAX-oriented datasets, labels, training and model artifacts.
3. **Rust live trading core** — market data, risk, OMS, execution, reconciliation, journal/recovery and replay.

The repository is intentionally smaller than an institutional platform. It borrows proven abstractions from the open-source ecosystem instead of reimplementing Polars, Arrow, PyTorch, NautilusTrader, exchange SDKs, PostgreSQL, Prometheus or Terraform.

> `pg-tsy-core` is an independent PG project inspired by common modern quant architecture patterns. It is not TSY Capital source code and is not affiliated with TSY Capital.

## Design rule

**Research proposes. Rust disposes.**

Python can produce a signal, model artifact or feature set. Only the Rust live core may turn that into order intent after risk checks and ownership/reconciliation rules.

```text
S3 / Parquet / Arrow
        |
      Polars
        |
   +----+--------------------+
   |                         |
Factor Research          LOB / ML Research
Python / Qlib ideas      PyTorch / JAX
Alpha mining             DeepLOB/TLOB ideas
   |                         |
   +-----------+-------------+
               |
          Signal Contract
               |
       Rust Trading Core
 MarketData -> Risk -> OMS -> Execution
                    |       |
              Reconcile   Journal
                    |       |
              Binance   Hyperliquid
```

## Repository map

```text
contracts/                  Versioned cross-language contracts
research/                   Python research control plane
  src/pg_tsy/
    data/                    Parquet / Arrow / Polars data access
    factor/                  Factor definitions and registry
    ml/                      ML datasets / model artifact contracts
    signal/                  Signal creation and development store
rust/
  crates/
    pg-types/                Shared domain types
    pg-marketdata/           Sequence/book state primitives
    pg-risk/                 Pre-trade risk engine
    pg-oms/                  Order state machine
    pg-execution/            Execution adapter interface
    pg-reconcile/            Venue ↔ journal ↔ internal state reconciliation
    pg-journal/              Durable event journal
    pg-replay/               Deterministic event replay
    pg-core/                 Live/shadow orchestration binary
  adapters/
    pg-binance/              Binance/Portfolio Margin adapter boundary
    pg-hyperliquid/          Hyperliquid adapter boundary
docs/                       Architecture, ADRs, roadmap and runbooks
infra/                      AWS/Terraform deployment blueprint
deploy/                     Local container assets
```

## Safety invariants

- Live trading is **off by default**.
- Research code never submits exchange orders.
- Every strategy-created order must have an ownership identity.
- Manual positions are never silently adopted by a strategy.
- Unknown exchange/order state is fail-closed for **new strategy exposure**.
- Risk/reconcile failures must still permit explicitly safe reduce-only/emergency actions where configured.
- Journal intent is persisted before externally visible state transitions when practical.
- Restart/recovery must reconcile against venue truth before opening new exposure.
- A signal has an expiry. Expired signals cannot create new orders.
- Backtest, shadow and live use the same order/risk domain types where possible.

## Local bootstrap

Python:

```bash
cd research
python -m venv .venv
source .venv/bin/activate
pip install -e '.[dev,ml]'
pytest
python -m pg_tsy.cli demo-signal --asset SOLUSDT
```

Rust (requires a Rust toolchain):

```bash
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p pg-core -- ../../examples/signal.solusdt.json
```

Local PostgreSQL:

```bash
docker compose up -d postgres
```

## Recommended production shape on AWS

Keep V1 deliberately boring:

- **S3**: raw/normalized market data and model artifacts.
- **Parquet + Arrow + Polars**: research data plane.
- **EC2 / AWS Batch**: research/training jobs; GPU only when needed.
- **Dedicated EC2**: Rust live core, deployed as a systemd service or a tightly controlled container.
- **RDS PostgreSQL**: durable metadata, signal registry, ownership records and reconciliation checkpoints.
- **Secrets Manager**: venue credentials.
- **CloudWatch + Prometheus/Grafana**: logs/metrics/alerts.

Do not introduce EKS/Kafka until measurements prove they solve a real bottleneck.

## Roadmap

See [`docs/ROADMAP.md`](docs/ROADMAP.md). The first production milestone is **shadow parity**, not real-money execution.
