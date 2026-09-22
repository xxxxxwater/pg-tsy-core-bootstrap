# First-version release readiness — verified 2026-09-22

> **Result: NO-GO for unattended/live trading and for a claimed three-venue end-to-end release. A `v0.1.0-rc.1` research/shadow-only prerelease is a candidate, not yet accepted or published.** This review inspects merged code, GitHub PR history and Actions evidence; it has not authenticated an exchange account, executed a real order, built a production image or deployed a server. Do not interpret documentation commits as release artifacts.

## 1. Review provenance and CI

The starting `main` commit was `293ba6296e7af47982b3df7c8b253cfe7c3ab3df`; the final audit/documentation commit must be rechecked independently. [PR #10](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/pull/10), merged as `bbbd2315f319114c0880475c59781607325eb2d9`, integrated the divergent legacy observability and Python/Rust simulation branches while preferring `main` on overlapping hunks with `-X ours`. PRs #8/#9 had overlapping simulation history; PR #1 added observability. Preserving merged Git ancestry does **not** prove the two simulation implementations are behaviorally equivalent or that an observability library is registered in a daemon. `55a42ba` consolidated an outdated README; this audit corrects its actual-runtime statements.

| Evidence | Exact baseline SHA | Latest observed result | Does NOT establish |
| --- | --- | --- | --- |
| [GitHub CI run 35719111159](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111159) | `293ba6296e7af47982b3df7c8b253cfe7c3ab3df` | **7/7 successful jobs**, confirmed via job results: Rust workspace fmt/Clippy/tests; Rust feature Hyperliquid/IBKR including pg-core IBKR compile and Telegram compile; Python Ruff/pytest; Binance/Jev contracts; Cargo.lock; Compose configuration | Final post-audit commit CI, container build/start, private venue authentication or external trading correctness |
| [PostgreSQL fenced-ledger run 35719111158](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111158) | Same baseline SHA | **fenced-ledger successful**: real isolated PostgreSQL tests for migration/ledger/settlement/replay/fencing plus fault regressions | Real exchange all-order/account-wide cursor, positions, fees or production recovery |
| Repository release/branch inspection | At start of audit | GitHub Releases API returned `[]`; `main` branch protection reported `false` | Status at some future date; signed builds or release governance |

**Always verify complete Actions results for the final exact release SHA**, not an ancestor. No tagged version, published Release, Docker image digest, full deployment or live acceptance was created by this audit.

## 2. Source-verified capability and gaps

| Area | Proven by reading merged source | Required before corresponding release |
| --- | --- | --- |
| Research/simulation | Python research, batch environment, causal replay, Rust `pg-sim` JSONL kernel and advisory Jev logic exist | Reproducible clean build and Python/Rust simulator parity across PR #8/#9/#10 overlap; fee/latency claims only after real fills |
| Strategy | Portable feature planning, normalized feeds, one decision dispatcher per definition and common risk/OMS path | Real `position_view()` currently has `average_entry_price=None`, `unrealized_return=None`, `peak_return=None`, `filled_entries=0`; return-based exits cannot be claimed live |
| Shadow | In-process ShadowExecutionAdapter for all three venue identities; no real execution in shadow | Postgres-backed order/fill/OMS/restart smoke, no-key proof, unsupported Binance feed failure and full shadow reconciliation evidence |
| Hyperliquid paper/live | `live_daemon` constructs real SDK adapter and searches stable `cloid`; paper requires Testnet | Authenticated segregated account, observed order/fills/fees/cancel/restart, emergency flatten and fault tests |
| IBKR paper/live | `live_daemon` constructs real TWS adapter and uses stable `order_ref` recovery | **Fail-closed proof of paper account identity before paper orders**; ordinary stock reduce-only only software enforced; manual ownership and race/emergency tests |
| Binance PM | Private stream/history parsers, isolated probes, order-level immutable fill/settlement components | No daemon PM runtime market feed or real execution registration; needs full signed all-order history/cursor/positions/fees and PM risk/emergency integration |
| Recovery | PostgreSQL lease/fencing; journal-before-adapter; real daemon initial+periodic `recover_ambiguous` and `reconcile_once`, sticky entry/topology guard | Real external accepted POST lost ACK, missing lookup, partial fill, kill-9, DB failure, stale lease/failover and no duplicate exposure evidence |
| Monitoring/control | Wired `/healthz`, `/readyz`, `/metrics`, `/admin/reload` and independent `pg-observability` crate | `/v1/snapshot` and `/v1/events` **not wired to pg-core**; reload handler has no authentication; authenticated Telegram emergency path unproven |
| Release operations | Pinned Rust 1.98.1, Cargo.lock, CI and Compose syntax validation | Exact-SHA green, clean Docker build/start, immutable digest/source artifacts, signed/protected ref, independent review and rollback procedures |

## 3. Critical merge discrepancies

1. **Old shadow-only docs were wrong:** `main.rs` routes `shadow` to `daemon::serve` and `paper/live` to `live_daemon::serve`. Hyperliquid and IBKR real adapters are constructed in paper/live; Binance PM registration explicitly bails.
2. **Paper can create external orders.** Hyperliquid paper enforces Testnet; IBKR adapter construction lacks an independently verified paper-account assertion. `PG_LIVE_TRADING=false` and `RunConfig::routes_to_real_venue()==false` in paper do not make adapter.submit offline. Never connect paper execution to a live IBKR account.
3. **Partial three-venue parity:** Binance PM has no daemon market-data source or real execution registration; PM diagnostic preconditions belong in its isolated runbook, not global README or a purported live release.
4. **Observability merge without runtime integration:** `pg-observability` is a workspace member but not `pg-core` dependency/router; its snapshot/events API cannot be advertised as a deployed endpoint.
5. **Live trading exit-feature gap:** real quantity-only `PositionView` lacks authentic entry/mark/return fields. Missing features may suppress PnL-based exits; prevent any strategy depending on them from routing money until fixed and verified.
6. **Operational security:** unauthenticated `POST /admin/reload`, health default bind `0.0.0.0:8080` (production Compose host-loopback mapping helps but direct deployments differ), no evidenced Telegram emergency completion, no protected `main` on initial review.

## 4. Gate A — first research/shadow-only candidate

- [ ] Freeze exact code/docs commit SHA and obtain independent review of PR #10 `-X ours` overlap.
- [ ] Confirm all required CI and isolated PostgreSQL workflow jobs are **successful on that exact SHA**; do not infer success from baseline CI.
- [ ] Run a clean pinned-toolchain `cargo build --release`, feature build, `pg-sim` JSONL and Python `RustSimClient` parity/fixture smoke, strategy replay and dependency/license/secret review.
- [ ] Run PostgreSQL-backed `pg-core --serve` shadow smoke: fresh feed -> risk -> durable intent/journal -> shadow ACK/partial/fill semantics -> OMS/reconcile -> restart, plus stale feed, conflicting order/ownership, lease loss and unsupported Binance feed fail-closed.
- [ ] Verify `/healthz`, `/readyz`, `/metrics`, protected reload, Compose build/run and clean shutdown without any real account credentials; document observers not wired and other known limitations.
- [ ] Verify reproducible source archive/image digest and rollback; publish release notes restricted to **research/shadow only**.

**Only when Gate A has real recorded evidence** may an authorized maintainer tag exact SHA as `v0.1.0-rc.1` and create a GitHub prerelease explicitly excluding paper/live trading. The workspace's `version=0.1.0` is not a Git tag or a published release. Current connected GitHub actions permit docs/source editing but expose no Release-creation operation; this review did not publish a tag, Release or image.

## 5. Gate B — segregated paper/testnet per venue

- [ ] Hyperliquid: independently verify Testnet credentials/account and prove authenticated reads, cloid submit, observed fills/fees/cancel/positions, reconnect and recovery.
- [ ] IBKR: **enforce and verify paper account identity in code**, including external Gateway and environment overrides; paper/stock contract correctness, software reduce-only races, execution-history recovery and fees.
- [ ] Binance PM: complete production market feed, gated adapter, isolated PM account permissions, complete authenticated signed history + durable all-order cursor/fill/position settlement; do not promote read-only probes to trading authorization.
- [ ] Per venue test missing/lost ACK, missing immediate lookup, partial/late fill, cancel ambiguity, stale feed, manual ownership, database/lease/fencing loss, kill-9, owned-only emergency exit.

## 6. Gate C — real-money canary/unattended

- [ ] Independent operator signoff for separately isolated keys, symbol and capital limits, explicit start and kill switch.
- [ ] Prove exchange-authoritative fills/fees/positions -> OMS/journal/cursor under fenced durable recovery; no duplicate exposure or improper SAFE_HOLD release.
- [ ] HALT first, owned-only cancel, venue-appropriate risk-reducing flatten, **observed completion** or incident escalation under ambiguous state; never touch manual/unowned positions.
- [ ] Record real latency p95/p99, costs/markouts, drawdown, reproducible audit, rollback and operations acceptance; Jev/simulation do not bypass hard risk or supply this evidence.

## 7. Non-interference

No real credentials in GitHub Actions, issues, docs or bot logs. All existing Binance PM/Freqtrade live services and manually owned positions are **out of scope**. No CI or release process may migrate, restart, reconcile or modify them. Publication is conditional on real evidence, not enthusiasm for a version number.
