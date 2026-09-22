# Isolated HFT milestone and release boundary — 2026-09-22

**Release state: BLOCKED for Binance real-order routing and unattended trading.** This document supersedes older point-in-time descriptions where they conflict with the actual code. A green CI validates the exercised paths, not account permissions, market profitability or operational sign-off. Nothing in this change enables live order routing, changes any production host, modifies an incumbent Binance PM/Freqtrade bot, or adopts manually owned positions.

## What is actually implemented

- `pg-core/src/live_daemon.rs` already has a periodic Tokio reconciliation tick which runs `recover_ambiguous` then `reconcile_once`, freezes new exposure on any non-clean cycle, and checks lease heartbeat. This tick does **not** prove a full Binance live cycle: `build_real_adapter_registry` intentionally rejects Binance PM registration. `pg-core/src/daemon.rs` remains the shadow path.
- `pg-store/src/fill_ledger.rs` already stores deduplicated, ownership-validated individual execution fills with fees and a fenced journal transaction. `recorded_fill_quantity` is read-only. Crucially, there is **no proven authenticated Binance trade-history -> ledger -> atomically verified OMS quantity + position update in the live daemon**. `pg-orchestrator::validate_snapshot` intentionally rejects a remote/local filled-quantity mismatch rather than inventing trade identities. Do not weaken it to make a test green.
- `pg-orchestrator/tests/lost_ack_fault.rs` injects three *isolated* faults against the actual durable dispatcher: exchange accepts but ACK is lost, order history visibility is lost, and fencing is invalid before dispatch. The mock counts external POSTs and asserts no duplicate POST from recovery. This is not a real-exchange kill-9/partition proof.
- `pg-binance/src/user_transport.rs` already implements PM listen-key/private WS protocol, bounded sink and reconnect-required signals. `pg-binance/src/bin/pm_user_probe.rs` adds an opt-in **read-only diagnostic**: a maximum of three sessions, mandatory reconcile-required observation, aggregate counters only, no OMS updates, no new/cancel orders. Its environment guard has a CI unit test. **No real authenticated PM stream session is claimed to have been run by this change.** The diagnostic `ReconcileRequired` event only signals work needed; it is never proof that REST reconciliation succeeded.
- `research/src/pg_tsy/sim/causal.py` supplies causal one-order quote replay, explicit outbound/model delay, front/back *assumed* queue bounds, opposing trade-volume partial fills, cancellation-effective timing, Decimal fee accounting and follow-up markouts. `compare_actual_fills` requires caller-provided authenticated, deduplicated trade IDs and quote-converted fees. Without real input it refuses to produce a calibration comparison. This is a bounded research model, **not** a full L3 venue emulator or a realized portfolio PnL backtest. The existing `pg-sim` Rust matching kernel remains a separate contract simulator.
- `research/src/pg_tsy/sim/challenger.py` compares four *paired* research policies on the same tape/queue/fees: rules, statistical filter, pinned Jev risk filter, and Jev with confidence gate. It makes delayed model answers shift the earliest quote arrival, drops stale/unpinned responses, and calculates held-out Brier/ECE and p95/p99 helper statistics. No Jev inference is performed by the research evaluator; callers must supply authenticated, timestamped model observations. Vendor confidence is not assumed to mean trade win probability.
- `research/src/pg_tsy/sim/release_gate.py` computes **review eligibility only**. It explicitly rejects missing causal/recovery evidence, calibration and latency violations, nonpositive/under-baseline net performance, excessive drawdown, absent independent reviewer and missing canary operator authorization. All evidence fields are *claims supplied by a caller*: the helper does **not** cryptographically verify a signature, validate data provenance, configure a deployment or grant trading permission.

## User Stream diagnostic — isolated operator only

Run only with an independently permission-verified, segregated account, the *existing* authorized API key provisioned through a secret store, an operator-selected nonsecret account-scope identifier, and these exact safety flags:

```text
PG_RUN_MODE=shadow
PG_LIVE_TRADING=false
PG_PM_READ_ONLY_PROBE_APPROVAL=APPROVE_ISOLATED_READ_ONLY_PROBE
PG_ISOLATED_ACCOUNT_SCOPE=<nonsecret isolated account label>
PG_BINANCE_PM_API_KEY=<secret provided out-of-band, never committed>
```

The diagnostic is `cargo run -p pg-binance --bin pm_user_probe` from `rust/`, requires no trading mutation capability, and outputs only session number, event count and reconciliation-required status. It does not independently verify that the key is read-only; the operator must do so before launch. A session/reconnect result remains **diagnostic**, not an OMS fill or release gate. CI never loads private keys or invokes this executable against Binance.

## Non-negotiable P0 production blockers

1. Segregated test account, permissions, symbol/mode/account-wide margin and collateral verification; authorized host must successfully read signed order history and private stream. Read-only first; preserve every manual position's independent ownership. No credentials or account identifiers in CI/logs.
2. Continuous **venue-authenticated** trade-ID/fee pagination and watermark collection, persisted across restart; full page coverage and WS/REST dedup, including cancel/fill races. For each owned order, atomically reconcile ledger sum, authoritative cumulative order quantity, OMS state, journal and derived position, without overwriting conflicting evidence. Any absent history or discrepancy remains SAFE_HOLD.
3. Verify retry/rotation/reconnect against the actual Binance PM private stream on the isolated host. If the stream ends or listen key expires, gate new exposure until authenticated REST history and order/position truth are complete. Never mark ready merely because WebSocket reconnects.
4. Wire Binance PM through the existing risk -> leased persisted intent -> OMS -> execution path **only in a separate staging configuration**; verify all reject/Unknown paths and live order-status queries. A lost ACK cannot trigger a fresh identity or second order. Prove the accepted-POST/lost-ACK, late fill, failover and database outage windows with real API or a sufficiently faithful staging environment before canary.
5. Wire authenticated, idempotent `/emergency_exit` into HALT -> cancel strategy-owned working orders -> exchange-truth refresh -> reduce-only flatten -> completion verification, with timeouts and escalation. Never flatten manually owned/unknown quantities or claim success on API ACK alone. This command is not fully accepted in this milestone.
6. Feed **genuine** historical market events, exact per-account effective commission/rebate/funding, complete signed fills and quote conversion into replay; freeze a dataset digest and show front/back queue sensitivity against measured fills, missing-data coverage and realized net markouts.
7. Perform at least three strictly forward walk-forward windows with no lookahead, matched capital/cost/limits/coverage and uncertainty intervals; report four-arm net results and Jev ablation. Stratify fill-conditioned markout; prove positive *incremental* out-of-sample net results before promoting challenger.
8. Require protected CI, external human code review, immutable data/model/fee provenance attestations, operator-owned access and kill-switch controls. Evidence helper `eligible_for_review=true` must **never** automatically toggle live. Separate signed authorization is required for a tiny, loss-bounded allowlisted canary only after all prior gates pass.

## Acceptance evidence commands

```bash
cd rust
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p pg-binance --all-targets
# Run existing independent PostgreSQL workflow against an isolated PostgreSQL service.
cd ../research
ruff check .
pytest -q
```

Do not run real-order or private-stream probes in standard CI. A compiler/test pass is evidence for code paths exercised, not a real PM endpoint or realized profitability. Check the latest `main` GitHub Actions run for the exact final commit before recording acceptance.
