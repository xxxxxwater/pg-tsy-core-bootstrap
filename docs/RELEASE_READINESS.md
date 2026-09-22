# Release readiness and three-venue acceptance — 2026-09-22

> **A Research/Shadow source prerelease EXISTS:** [`v0.1.0-rc.1`](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/releases/tag/v0.1.0-rc.1), published on 2026-09-22, is pinned to `ce7defe22c6b385aa0b1da14f7f45611ae60e136`. It has no attached binary/image and is not a production or unattended/live trading release. This tag predates post-RC1 changes including the real `pg-sim` JSONL binary and HTTP admin hardening; **do not move, overwrite, or claim they are part of RC1**. Three-venue live/unattended: **NO-GO**.

## 1. Evidence ledger — do not mix SHAs

| Source | Exact SHA | Verified meaning | What it does not prove |
| --- | --- | --- | --- |
| [Initial audit CI](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111159) | `293ba6296e7af47982b3df7c8b253cfe7c3ab3df` | 7/7 jobs successful (Rust, Python, venue SDK contract, Jev, Binance, lockfile, Compose) | Later source changes or actual venue execution |
| [Initial PostgreSQL CI](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111158) | Same SHA | Fenced-ledger database tests successful | Exchange-authenticated account-wide fill/fee/position truth |
| [Published RC1](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/releases/tag/v0.1.0-rc.1) | Tag target `ce7defe22c6b385aa0b1da14f7f45611ae60e136` | Published source-only research/shadow prerelease; release notes explicitly excluded working JSONL executable | Docker image, paper/live approval or subsequent main changes |
| [Simulator bridge CI](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35724050942) | Integration candidate `2a0fd9824bece7a0badbcc347c7f52ec890087c8` | Real Rust executable and Python subprocess contract verified on candidate | Exchange adapter acceptance |
| [Latest pre-auth main CI](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35724440246) and [PostgreSQL](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35724440120) | `726a28c464aaf354e587c50fbc92e205a961ca33` | Both source CI workflows completed successfully | This security patch or deployment |
| Admin reload hardening | Separate `hardening/authenticated-admin-reload` branch; exact SHA to be recorded after final tests | Authentication and denial must be proven by full CI and explicit TCP regression | TLS, secret delivery, Telegram emergency or live trading readiness |

PR #10 merged diverged observability/simulation history with `-X ours` on overlapping hunks; preserve independent semantic review of PR #8/#9/#10 behavior. Git ancestry and broad workspace CI alone cannot establish feature completeness. For post-merge status see [STATUS](STATUS.md) and [engineering automation](ENGINEERING_AUTOMATION.md).

## 2. Source-verified architecture and current blockers

| Scope | Actual source | Unfulfilled acceptance |
| --- | --- | --- |
| Run mode | `main.rs`: shadow -> `daemon::serve`, paper/live -> `live_daemon::serve` | Paper/live invoke real adapters and can send external orders; no automatic shadow fallback |
| Research and simulation | Python research/replay; `pg-sim` library and post-RC1 JSONL executable, Python subprocess bridge | Causal fixture parity, real-fill/fee calibration and an independent simulation merge review beyond unit CI |
| Hyperliquid | Default feed, real SDK adapter, stable `cloid`, testnet guard in paper | Isolated authenticated order, partial/late fill, fees, cancel, disconnect, kill-9, restart, emergency and ownership proof |
| IBKR | TWS feed (opt-in), real adapter, `order_ref`, execution recovery | Must prove **paper account identity fail-closed** independent of gateway mode/port overrides; equity reduce-only races and fee/history completeness |
| Binance Portfolio Margin | PM parsers, signed history, isolated stream/diagnostics and order-level settlement | Real daemon feed/adapter explicitly absent; account-wide signed cursor, fills, fees, positions, PM risk and emergency not complete |
| State/strategy | Journal-before-submit, PostgreSQL lease/fencing, ambiguous recovery, periodic reconcile | Complete venue truth and crash/failover no-duplicate-exposure evidence; real quantity-only `position_view` lacks entry/return/peak/fill count |
| Observability | `/healthz`, `/readyz`, `/metrics`; separate `pg-observability` crate | `/v1/snapshot` and `/v1/events` not mounted; no claim of deployed endpoints |
| Operator control | HTTP reload with post-RC1 default-deny `PG_ADMIN_TOKEN` branch hardening | Listener defaults `0.0.0.0`; managed secret delivery, network isolation and TLS remain operator tasks; Telegram authenticated `/emergency_exit` not accepted |

## 3. Gate A: historical source prerelease versus follow-up engineering

`v0.1.0-rc.1` is already published. Do **not** retroactively label unfinished gates as complete or reuse its tag for a changed tree. Before producing a *new* prerelease on a new tag, require:

- [ ] Exact new candidate SHA and independent PR diff review; audit PR #10 simulation overlap.
- [ ] All source CI, new non-skipped Rust executable/Python simulator bridge, isolated PostgreSQL CI, pinned clean feature/release build and license/secret review green on that exact SHA.
- [ ] PostgreSQL-backed `pg-core --serve` SHADOW lifecycle smoke: real normalized feed -> risk -> durable intent -> shadow ACK/partial fill -> OMS/ledger/reconcile -> stop/restart -> no duplicate submission. Use no real credentials.
- [ ] Exercise lease loss, DB outage, stale feed, unknown orders/ownership, unsupported Binance feed fail-closed, and container build/start/shutdown with recorded logs and source/image digest.
- [ ] Verify anonymous health endpoints still work and `/admin/reload` denies absent/wrong credentials without dispatch; keep reload disabled by default. Track authentication limits in [ADMIN_RELOAD_SECURITY](ADMIN_RELOAD_SECURITY.md).
- [ ] Immutable source release notes and rollback. Explicitly exclude real venue/Paper/Live acceptance from any Research/Shadow prerelease.

## 4. Gate B: segregated paper/testnet, one exchange at a time

- [ ] Hyperliquid: prove network and account identity, authenticated complete fills/fees/cancel/positions, stable client IDs and restart recovery.
- [ ] IBKR: fail-close on paper-account identity in code for every gateway/override path *before any order*; audit software-only stock reduce-only, ownership and external executions.
- [ ] Binance PM: implement full real feed + registered gated adapter, authenticated account-wide trade/order/position cursor, fees, settlement, available collateral and PM-specific risk controls. Do not infer readiness from read-only probes.
- [ ] Each venue: deliberately inject lost ACK, stale stream, immediate lookup missing, late/partial fill, cancel ambiguity, database/lease/fencing failures and kill-9. Confirm no duplicate exposure and no modification of manual/unowned positions.

## 5. Gate C: real-money canary/unattended, separate authorization

- [ ] Independent named operator approval, isolated keys/accounts, symbols, size limits, explicit startup and manual kill switch.
- [ ] Exchange-authoritative fills/fees/positions reconciled into OMS, journal and durable cursors; zero unsafe duplicate exposure in fault tests.
- [ ] Emergency HALT-first -> cancel owned resting orders -> refresh venue state -> only appropriate risk-reducing flatten -> confirm fills/flat or preserve SAFE_HOLD and escalate. **An ACK is not a confirmed flat state.**
- [ ] Audit p95/p99 actual order latency, fees and markout, bounded loss, incident drills, rollback and venue-specific acceptance artifacts; Jev model probabilities do not bypass risk.

## 6. Non-interference and release governance

No exchange secrets in Actions, docs, issues or logs. No CI release job should connect to trading accounts. `main` contains integration work and its green tests are not a deployment credential. Existing Binance PM/Freqtrade production services and manual positions remain separate and untouched. Never enable real orders, move an old tag, claim a Docker image exists or silently clear SAFE_HOLD as a side effect of a merge.
