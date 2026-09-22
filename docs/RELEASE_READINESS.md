# First-version release readiness — 2026-09-22

**Decision: NO-GO for an unattended/live-trading release, and NO-GO for claiming a verified three-venue end-to-end execution release.** This is a source-and-GitHub-CI review, not a real account test. A non-trading research/shadow *candidate* may be prepared separately after its exact commit passes CI and its documented smoke tests are recorded. Do not call a Markdown commit, a Cargo workspace version or a Docker Compose syntax check a published GitHub Release.

## Review scope and provenance

- Baseline: `293ba6296e7af47982b3df7c8b253cfe7c3ab3df` on `main`, before this documentation audit; use the final documentation commit SHA for any subsequent candidate.
- Merge history: PR [#10](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/pull/10), merged as `bbbd2315f319114c0880475c59781607325eb2d9`, integrated two divergent legacy branches using `-X ours` on overlap; preservation of branch history is not a semantic parity test. PRs #8/#9 added overlapping simulator paths; PR #1 brought an observability module/document. README consolidation `55a42ba665d6b10626d9f92e42ec5e3794b0e1c2` introduced claims which no longer matched runtime source. PR #10 has no discussion comments in the retrieved timeline.
- Evidence: actual `main.rs`, `daemon.rs`, `live_daemon.rs`, `pg-runtime`, `pg-core/Cargo.toml`, `pg-observability`, `health.rs`, `rust/Cargo.toml`, Compose and `docs/*`; latest inspected GitHub workflow run [ci #35719111159](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111159) and PostgreSQL [#35719111158](https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35719111158). The first six CI jobs had passed at observation time; `rust-integrations` was still running. PostgreSQL fenced-ledger job had passed. These runs target the **pre-audit** commit and cannot validate a later documentation commit automatically.
- At inspection the Releases API returned an empty list; repository `main` branch protection reported disabled. GitHub releases/tags or branch protection may have changed after this snapshot; re-query before release.

## Capability / evidence matrix

| Area | Observed code/test evidence | Missing acceptance | Gate |
| --- | --- | --- | --- |
| Build and test | Cargo workspace Rust fmt/Clippy/tests, Python Ruff/pytest, lockfile, Binance/Jev adapter tests; isolated PostgreSQL fill/fencing job passed on inspected baseline except integration job was still running | Exact final release SHA fully green; container build and runtime smoke on clean machine not established | Pending |
| Research and simulation | Python batch environment, `pg-sim` deterministic JSONL kernel, causal replay and challenger code; merged PR #10 | Cross-branch simulator semantic parity/regression matrix, build/package smoke and reproducible fixture outputs | Pending |
| Hyperliquid | Market-data feed and real adapter constructor; `cloid` recovery contract; `paper` requires Testnet | Authenticated segregated account, isolated full order/fill/fee/cancel/restart/e-stop proof | Block live |
| IBKR | TWS/Gateway feed and real adapter constructor; `order_ref` recovery; software reduce-only opt-in | **Paper-mode account enforcement** in real-adapter path; authenticated paper and live isolation; native reduce-only absent for ordinary equities; real partial-fill/restart/emergency proof | Block paper/live order runs |
| Binance PM | Parser/WS/anchored history probes, order-level ledger and settlement primitives | Live market-data source and execution registration absent; account-wide authenticated history cursor + OMS/position/fill settlement, full recovery/stop/e-stop acceptance | Block real runtime |
| Common execution | Journal-before-dispatch and fencing checks; live periodic `recover_ambiguous`/`reconcile_once`, entry guard | End-to-end mixed-venue proof of no duplicate exposure under lost ACK, DB outage, kill-9 and failover; venue-appropriate order/position evidence | Block unattended |
| Live position features | Strategy quantity flows into `position_view()` | Live avg entry, fill count, unrealized and peak-return fields currently unset, affecting rules that consume them | Block affected strategies |
| Control and monitoring | `/healthz`, `/readyz`, `/metrics`, reload HTTP; separate `pg-observability` crate | Snapshot/events not in `pg-core` dependency/router; Telegram emergency control not proved wired; reload handler lacks authentication and binds `0.0.0.0` by default (Compose publishes host loopback) | Block advertised integration/public exposure |
| Deployment governance | Pinned toolchain/lockfile, Compose config job | Real container launch, immutable image digest/SBOM, rollback, signed artifacts, protected `main`, independent review and final-sha checks | Block production release |

## Highest-priority discrepancies uncovered by the merge audit

1. **Stale documentation could invert safety expectations.** `main.rs` routes shadow to `daemon::serve` and paper/live to `live_daemon::serve`. Older `README`, `EXCHANGES`, `STATUS`, `PRODUCTION_RUNTIME` and `ROADMAP` said paper/live are refused and no real adapters are constructed. Cross-reference source before marketing or operations.
2. **Paper mode has external side effects.** `live_daemon::build_real_adapter_registry` constructs Hyperliquid/IBKR real execution adapters. It rejects Hyperliquid paper unless network is Testnet, but does not itself prove IBKR paper account/Gateway mode. `RunConfig::routes_to_real_venue()` returning false in paper is not proof that external POSTs cannot happen. Do not run paper against a live IBKR session.
3. **Binance is a target venue, not equivalent integration.** `build_real_adapter_registry` explicitly bails for `BinancePm` and no runtime market-data source is wired. Keep isolated PM probe instructions in the PM-specific runbook and don't imply its checks authorize trading.
4. **Observability is merged but not integrated.** `pg-observability` is a workspace member, but `pg-core/Cargo.toml` does not depend on it. The running `health.rs` router only serves health/ready/metrics/reload; `/v1/snapshot` and `/v1/events` are not currently `pg-core` routes. The PR #10 merge preserved code, not a proven end-to-end integration.
5. **Live decision feature deficit.** The real daemon's `position_view()` carries quantity but initializes `average_entry_price`, `unrealized_return`, `peak_return` to `None`, and `filled_entries=0`. Do not claim live PnL-based exit parity with shadow.
6. **Release controls absent or unverified.** No GitHub Release at review, unprotected main, and no evidence of a successful signed build/deploy or all-venue fault-injection acceptance. Source-only tests cannot fill these gaps.

## Release gates

### Gate A — research/shadow candidate (non-trading)

- [ ] Freeze an exact commit with comprehensive documentation; do not tag a moving branch.
- [ ] Verify the exact SHA's complete Rust/Python/feature/Postgres CI is `success`, not `in_progress`, skipped or inherited from a parent.
- [ ] Run clean-machine Cargo release build and `pg-sim` JSONL batch smoke, Python `RustSimClient` parity/fixtures, strategy replay and Postgres-backed shadow daemon smoke.
- [ ] Check `/healthz` and `/readyz`, failure behavior for an unsupported Binance subscription, simulated OMS/journal lifecycle and shadow feed staleness, and confirm no real order keys or trading endpoints are needed.
- [ ] Document expected Shadow limitations (especially fill-to-OMS reconciliation, pending observability integration) and distinguish smoke evidence from live-venue acceptance.
- [ ] Inspect dependency/security licenses, secrets, image digest and reproducible source archive; obtain independent code review of merge overlaps.
- [ ] Only then publish a clearly labeled `v0.1.0-rc.1` **prerelease: research/shadow only**. The workspace currently uses `0.1.0`, but that is not a tag or Release. Do not mark `latest` stable until scope is proven.

### Gate B — per-venue paper/staging

- [ ] Hyperliquid: enforce verified Testnet + isolated credentials and real testnet order/fill/fee/read/cancel/reconnect reconciliation.
- [ ] IBKR: verify paper Gateway/account identity through authenticated API and prohibit submission when paper identity cannot be proven; test software reduce-only race behavior.
- [ ] Binance PM: integrate authenticated feed + real execution/position/order history and durable cursor; dedicated account segregation and permissions.
- [ ] Run each adapter's idempotency, partial fill, cancel ambiguity, stale market data, manual ownership, lease fencing, DB outage and kill-9 fault matrix with signed evidence.

### Gate C — real-money canary and unattended

- [ ] Independently approve per-account capital/symbol limits and isolated secrets; require explicit operator signed go/no-go.
- [ ] Prove live execution -> fills/fees -> OMS/positions -> journal/cursor -> reconciled `SAFE_HOLD` release in a fenced atomic design appropriate for that venue.
- [ ] Validate halt, owned-order cancellation and actual reduce-only emergency flatten **with completion evidence**; manual/unowned exposure untouched.
- [ ] Record venue-level audit trails, p95/p99 latency, markouts/net fees, post-crash no-duplicate evidence and rollback/recovery plan.
- [ ] Only then discuss a live-capable release; an isolated Jev diagnostic or synthetic backtest is not such evidence.

## Version and publishing commands (instructions, NOT executed)

After Gate A is fully evidenced, an authorized maintainer may create an annotated `v0.1.0-rc.1` tag at the reviewed exact SHA and publish a GitHub **pre-release** with the description **Research/shadow only — no paper/live trading acceptance**. Verify GitHub Actions run on the actual tag/commit, source archive and release assets; reject any unreviewed drift. This document does **not** create a tag, Release, Docker image or server deployment. Current tool access can update repository source/docs but does not expose a GitHub Release-creation action. Until then, link users to the repository and this acceptance document, not a fictional release page.

## Non-interference

Never use CI secrets for real trading accounts; don't paste keys in issues or runbooks. The existing Binance PM/Freqtrade bot and manual positions are a separate production system and must not be migrated, stopped, reconciled or modified by this repository's release process.
