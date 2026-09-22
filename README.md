# PG-TSY Core — Quant Research & Execution Infrastructure

Contract-first Python/Rust quantitative trading monorepo for one engineer working with AI coding agents. **Research proposes; Rust disposes.** Python prepares features, models, candidate signals and reproducible evaluations; deterministic Rust policy, risk, OMS and execution contracts retain authority over orders. Neither model confidence nor a passing CI workflow authorizes real trading.

> Independent PG project, not TSY Capital source code or an affiliated product. Current release status: **research/shadow and isolated verification; unattended real-money Binance PM trading is BLOCKED**. The repository is not a demonstrated profitable HFT product. See [isolated HFT acceptance](docs/ISOLATED_HFT_ACCEPTANCE_2026-09-22.md) and [status](docs/STATUS.md) for evidence and remaining blockers.

## What is implemented

| Layer | Current capability | Important boundary |
| --- | --- | --- |
| Research | Python factor/ML pipelines, batch environments, walk-forward/parameter research, versioned signal and model artifacts | Research cannot submit venue orders. Batch-training returns are not HFT backtest or realized PnL. |
| Strategies | Rust normalized trades/BBO/L2/candles, feature provider registry, portable rule graphs and a strategy state machine | Missing required live features fail loading; legacy and policy paths do not both submit for one definition. |
| OMS / execution | Durable intent-before-POST, stable client order IDs, partial-fill accounting, unknown-submit lookup and recovery; Hyperliquid/IBKR adapter contracts | SDK-capable adapters are not proof of completed real-exchange staging acceptance. Unknown outcome never triggers blind resubmission. |
| Data & persistence | PostgreSQL journal, lease/fencing, order records, ownership and reconcile reports, immutable per-trade fill ledger | Historical coverage and exchange-truth positions must be verified before releasing SAFE_HOLD. |
| Binance PM | Strict public market-data and private-stream parsing, signed trade-history decoder/collector, owned-order history verifier, isolated atomic complete-order fill/OMS settlement and a separately opted-in read-only WS diagnostic | No authenticated isolated-account acceptance; no atomic account-wide history cursor and position settlement; real Binance execution stays unregistered in the daemon. |
| Simulation | Rust `pg-sim` order-semantics reference plus Python causal replay for bounded queue uncertainty, latency, partial fills, cancel races, fees and fill deviation comparison | Neither is venue-accurate L3 matching without real captured order-book and execution evidence. |
| Jev challenger | Advisory model contract, matched rule/statistical/Jev/Jev+confidence research, calibration and latency/markout measurements | Advisory only: model decisions cannot bypass hard risk or enable order routing; actual edge has not been established. |
| Operations | Health/ready/metrics, operator command contracts, continuous recover/reconcile scaffolding and failure-injection tests | Emergency flatten and real authenticated full-cycle reconnect/settlement still require isolated end-to-end proof. |

## Architecture

```text
Historical data / live normalized market events
                    |
       Python research and causal replay
                    |             Jev advisory (optional)
         signed/versioned artifacts         |
                    +-----------+------------+
                                v
                         Rust feature engine
                                |
                   strategy / deterministic policy
                                |
                 risk + freshness + ownership gates
                                |
                   durable intent -> OMS -> journal
                                |
             execution adapter / shadow venue by mode
                                |
               exchange ack / trades / positions
                                |
           verified history -> fenced fill settlement
                                |
                reconcile / checkpoint / SAFE_HOLD
```

The diagram includes target paths. In particular, **verified history → OMS → account-wide cursor/position in one live transaction is not wired end-to-end**, and a read-only WS probe is not a reconciliation service. `pg-core --serve` is shadow-only; never confuse an adapter existing in source with an enabled production route. See [architecture](docs/ARCHITECTURE.md), [exchange contracts](docs/EXCHANGES.md) and [strategy documentation](docs/STRATEGIES.md).

## Repository layout

- `research/` — Python research, batch market environment, causal simulation, challenger comparisons and release-evidence tooling.
- `rust/crates/` — normalized market data, strategy, `pg-sim`, risk, OMS, execution, reconciliation, journal, PostgreSQL store, replay, controls and core orchestrator.
- `rust/adapters/` — Binance PM, Hyperliquid and IBKR venue boundaries.
- `strategies/`, `contracts/`, `data/replay/` — definitions, versioned interfaces and deterministic fixtures.
- `infra/`, `scripts/`, `.github/workflows/` — deployment blueprints, development scripts and CI.
- `docs/` — design decisions, operations, honest acceptance evidence and remaining release blockers.

## Verification and development

```bash
# Rust (pinned toolchain in CI)
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p pg-binance --all-targets
cargo test -p pg-hyperliquid --features sdk
cargo test -p pg-ibkr --features sdk

# Python
cd ../research
pip install -e '.[dev]'
ruff check .
pytest -q
```

The dedicated PostgreSQL workflow starts an isolated database and runs migration, deduplication, conflicting-trade, journal, fencing and complete-order settlement tests. The generic CI also checks Binance/Jev contracts, feature-gated SDK integrations, Compose config, Rust formatting/strict Clippy/workspace tests and Python Ruff/pytest. **All of this is code/test evidence, not Binance account authentication or real trading authorization.**

A private Binance diagnostic is deliberately separated from the daemon and requires an explicitly verified read-only isolated account, `PG_RUN_MODE=shadow`, `PG_LIVE_TRADING=false`, `PG_PM_READ_ONLY_PROBE_APPROVAL=APPROVE_ISOLATED_READ_ONLY_PROBE`, a nonsecret `PG_ISOLATED_ACCOUNT_SCOPE` and an out-of-band secret `PG_BINANCE_PM_API_KEY`. Run `cargo run -p pg-binance --bin pm_user_probe` from `rust/` only after permissions are checked. This probe creates no order, never mutates OMS, does not clear SAFE_HOLD and does not complete authenticated REST reconciliation. Do not put credentials in GitHub Actions.

## Release gates: do not enable real trading automatically

Before any independently authorized tiny canary, require a segregated real-account approval and authenticated API/WS reads; a durable genesis and complete all-order signed trade-history cursor; atomic fenced fill, OMS, position and cursor updates; reconnect/restart/kill-9/database/lease failure injection; tested owned-order cancel and reduce-only emergency flatten; real execution/fee/markout calibration; strictly forward walk-forward comparisons for four matched policy arms; p95/p99 latency, Brier/ECE, net PnL and drawdown evidence; independent review and separate operator-controlled deployment. A short page or WS reconnect by itself is never proof of complete history.

See [full acceptance and open blockers](docs/ISOLATED_HFT_ACCEPTANCE_2026-09-22.md), [storage/recovery](docs/STORAGE_RECOVERY.md), [roadmap](docs/ROADMAP.md) and [status](docs/STATUS.md). The incumbent Binance PM/Freqtrade production bot is not part of this repository migration and must not be altered by its CI.

## Git workflow

`main` is the integration branch. Historical feature branches whose HEAD is already an ancestor of `main` contain no unique commits and do not need a duplicate merge. Inspect ahead/behind and outstanding PRs before deleting any historical refs. Code changes must be committed with reproducible test evidence, and README/acceptance documents should change with meaningful functionality; documentation cannot certify tests or live deployments that were not performed.
