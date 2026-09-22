# Isolated HFT milestone and release boundary — 2026-09-22

**Release state: BLOCKED for Binance real-order routing and unattended trading.** Green CI validates only exercised code, not real account access, profitability, emergency execution or operational authorization. No production host or incumbent Binance PM/Freqtrade bot was modified; real order routing remains disabled.

## Implemented and CI-exercised components

- `rust/crates/pg-core/src/live_daemon.rs`: periodic `recover_ambiguous` and `reconcile_once`, fail-closed new-exposure gate, fencing/heartbeat monitoring. Binance PM is deliberately not registered by the real-venue adapter builder; these routines cannot yet operate an authenticated PM live account.
- `rust/adapters/pg-binance/src/user_transport.rs` and `src/bin/pm_user_probe.rs`: private listen-key/WS transport and separately opted-in read-only diagnostic, at most three sessions, mandatory `ReconcileRequired`, aggregate counters and no OMS/order mutations. No actual authenticated Binance account session was executed during this change. The operator must verify API-key read-only permissions out of band.
- `rust/adapters/pg-binance/src/trade_history.rs` and `src/history_reconcile.rs`: strict signed UM trade decoder, owned-order contract check, bounded anchored pagination, page/cursor continuity, trade-ID deduplication, fee preservation, and exact cumulative snapshot quantity validation. The validation function consumes caller-provided data: it does not itself authenticate an HTTP response, establish a complete historical anchor or persist a restart cursor. Its zero-baseline sum requires the supplied batch to contain the **full order trade history**; using it on only the most recent page after advancing a cursor will fail closed until a persisted pre-anchor baseline is verified. Binance's history lookback constraints require explicit coverage validation.
- `rust/adapters/pg-binance/src/history_fetch.rs`: **new** signed REST collection boundary using `BinanceRestClient::user_trades_page` and the same bounded ownership/quantity verifier. Enforces a nonzero externally verified anchor, forward-moving cursors, at most 32 pages, a mandatory final short page, and failure on REST outages/foreign trades; offline reader-fault tests use no credentials. This is NOT yet called by the daemon or PM probe and does not bind an authenticated account-wide order inventory, establish genesis, persist a watermark or release SAFE_HOLD. It is appropriate only for a separately verified complete-history anchor, not a resumed incremental cursor without audited prior-fill baselines.
- `rust/crates/pg-store/src/fill_ledger.rs`: individual immutable execution fills with order ownership, trade-ID deduplication, commission and fenced journal transactions; `recorded_fill_quantity` remains read-only.
- `rust/crates/pg-store/src/fill_ledger/settlement.rs`: isolated atomic settlement primitive `settle_complete_order_history`. For a single owned order it locks the lease and OMS row, validates a caller-supplied complete authenticated-history representation against all previously persisted trades, rejects altered fees, duplicates, overfills and terminal resurrection, then writes new fills, OMS cumulative quantity/state and journal entries in one PostgreSQL transaction. Retry of identical complete history inserts zero further fills. It does NOT authenticate signed responses on its own, advance an account-wide history cursor, update exchange-truth positions, clear SAFE_HOLD, or register in the live daemon. A conflicting batch rolls back all writes.
- `rust/crates/pg-store/tests/fill_settlement_pg.rs`: actual PostgreSQL 16 CI exercises partial -> full OMS settlement, identical replay, tampered-fee rejection, per-trade journal consistency, terminal rollback and fencing failure. `.github/workflows/postgres-fill-ledger.yml` requires this test alongside the pre-existing migration/dedup/conflict/fencing integration tests and strict Clippy; a missing database URL fails CI.
- `rust/crates/pg-orchestrator/tests/lost_ack_fault.rs`: injected acknowledged exchange POST with lost ACK, missing order lookup and invalid fencing; count-based assertions prevent a second POST under tested recovery paths. These tests are not a physical network partition or live-exchange restart experiment.
- `research/src/pg_tsy/sim/causal.py`: bounded research-only queue-bound/latency/partial-fill/cancel/fee replay and explicit actual-fill comparison interface, not exchange-accurate L3/HFT realized PnL. `challenger.py` evaluates matched rule/statistical/Jev/Jev+confidence policies on caller-provided observations. `release_gate.py` computes review eligibility, NOT any live authorization. Genuine historical fills, effective fees and forward walk-forward results have not been furnished or measured.

## Isolated PM private-stream diagnostic

Before attempting a diagnostic on an independently verified segregated read-only account, set `PG_RUN_MODE=shadow`, `PG_LIVE_TRADING=false`, `PG_PM_READ_ONLY_PROBE_APPROVAL=APPROVE_ISOLATED_READ_ONLY_PROBE`, `PG_ISOLATED_ACCOUNT_SCOPE` to a nonsecret label, and provision `PG_BINANCE_PM_API_KEY` through an out-of-band secret store. Run `cargo run -p pg-binance --bin pm_user_probe` from `rust/`. No private credentials, mutation capability or account identifiers belong in GitHub Actions. A reconnect is never equivalent to authenticated REST reconciliation.

## Remaining hard blockers before any staging live routing or canary

1. Segregated account and verified PM API permissions, margin collateral, position mode, symbol contract, manual ownership and secret handling; real authenticated signed order/history and WS reads in isolation. No account/session access was performed here.
2. Durable, externally proved genesis/history coverage and an account-wide order inventory; persist a monotonic trade-ID watermark, pre-anchor fill baselines, **all-order settlement and the cursor in the same fenced transaction**. Signed REST page collection is now available as an isolated component, but there is no persistent cross-order cursor or atomic cross-order ingestion. Missing, truncated, foreign or conflicting evidence must remain SAFE_HOLD. An order-level complete-history batch is not interchangeable with a post-cursor incremental account page.
3. Integrate actual User Stream reconnect/rotation, authenticated REST history and positions into the continuous live daemon; keep new exposure blocked throughout disconnect until complete reconciliation, not just successful WS reconnection.
4. Isolated staging-only Binance PM risk -> durable intent -> OMS -> execution adapter registration, stable client IDs, signed cancel/order lookups, shadow-to-staging gate and tests for accepted POST/lost ACK, partial fills, DB outage, failover, kill-9, lease fencing and manual intervention. Existing production adapters remain disabled for PM.
5. Real authenticated `/emergency_exit` HALT -> cancel owned orders -> exchange-truth refresh -> reduce-only flatten -> actual completion validation, bounded retry and escalation without touching manual or unknown exposure. This end-to-end acceptance is NOT completed.
6. Signed real fill/fee and full-market-data capture with provenance; actual effective maker/taker fee/rebate/funding, front/back queue bias and actual net markout calibration. No simulated training return is an HFT backtest return.
7. Minimum three strictly forward walk-forward windows, four matched policy arms and Jev ablation, Brier/ECE, p95/p99, fee-adjusted net performance, drawdown, missing-data sensitivity and uncertainty intervals. Independent human review and operator-signed tiny-canary authorization are separate from CI and Agent-generated evidence.
8. Protected CI, independent code approval, immutable data/model/fee artifacts and separate operator control of deployment and kill switch. CI green must never turn on trading automatically.

## Evidence commands

```bash
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p pg-binance --all-targets
# On isolated PostgreSQL with PG_TEST_DATABASE_URL configured (workflow makes it mandatory):
cargo test -p pg-store --test fill_ledger_pg --test fill_settlement_pg -- --nocapture
cd ../research
ruff check .
pytest -q
```

CI tests represent deterministic and PostgreSQL isolation evidence, not Binance authentication or real execution. Confirm green Actions on the final `main` commit before recording release results. **Do not deploy or enable real orders until blockers are independently closed.**
