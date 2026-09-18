# JEV × PG-TSY × Binance BTCUSDC perpetual — integration plan

**Status: design and isolated groundwork only; live trading is NOT enabled.**

Audit baseline (2026-09-18): `main` at `d7e719b0b2815a1069df9cb4e25c49946dead699`. This plan targets the existing `main` branch. The existence of this document neither proves positive alpha nor authorizes deployment or orders.

## Objective and accounting

Target a *measured* **1–8 bps NET realized PnL per completed round trip**, expressed as `(realized round-trip quote PnL / entry quote notional) × 10,000`. The target is a research hypothesis, not an assurance. Record each maker/taker fill, actual commissions/rebates and fee asset conversion, funding, execution price, inventory mark, exposure and unmatched partial fills. Do not count a one-way fill or unrealized gain as a completed trade. For a round trip, avoid double-counting spread/slippage already reflected in actual execution prices.

Scope is Binance Portfolio Margin **BTCUSDC USDⓈ-M perpetual**, NOT BTCUSDT, inverse BTCUSD or spot. Confirm that this exact contract is listed, available for the actual account/mode and supports the proposed API, TIF and fee schedule. BTC collateral is an account-risk dimension, not the USDC quote currency. Exchange-reported account maintenance, haircut and position state are authoritative; never infer them from just BTC mark price.

## Findings from current main (verified GitHub code / docs)

- `rust/adapters/pg-binance/src/lib.rs` currently consists of a boundary comment and `pub struct BinancePmAdapter;`: it is **not an implemented production adapter**.
- `docs/STATUS.md` and `docs/EXCHANGES.md`: Binance has no wired runtime market-data source; current `pg-core --serve` only allows shadow; continuous `reconcile_once` is implemented but **is not scheduled by the daemon**, so venue fills do not continuously update durable records. Real Binance orders cannot be represented as ready or tested merely because a venue enum/adapter crate exists.
- `rust/crates/pg-marketdata/src/lib.rs` already has normalized `TradeTick`, `BestBidAsk`, `L2Book`, timestamps, `SequenceTracker`; add venue-specific snapshot/diff bridging and freshness rather than copying another venue's sequencing semantics.
- `rust/crates/pg-types/src/lib.rs`: `Signal` already has TTL, model and feature metadata; `OrderIntent.client_order_id()` is `pg` plus 32 lowercase UUID hex characters, i.e. **34 chars**. Binance PM UM order `newClientOrderId` allows up to 32. Add a Binance-specific deterministic reversible full-UUID encoding (prefix `pg` + unpadded Base32 of 16 UUID bytes = 28 characters); do **not** modify stable IDs for Hyperliquid/IBKR. Retain exact mapping across durable records, venue reads and recovery.
- `rust/crates/pg-execution/src/lib.rs`: venue-neutral `ExecutionAdapter`, ambiguous `ExecutionError::Unknown`, venue order snapshots, cancellation and read-side recovery. Implement actual Binance queries, including historical fills, not only `open_orders()`.
- `rust/crates/pg-strategy/src/lib.rs`, `policy/` and `registry/`: route JEV output into existing single-owner decision path; never add an independent model dispatcher that can open duplicate positions.
- `AGENTS.md` forbids research-to-order shortcuts, bypassing Risk, guessing ownership, treating unknown as rejected, auto-adopting manual positions and enabling live by default. Preserve all invariants.

GitHub branch audit: `dev/live-venue-runtime-20260914` is 31 commits behind `main` / 0 ahead; `feat/runtime-observability-integrated` is 70 behind / 0 ahead; `feat/runtime-observability-v1` diverged (9 ahead, 77 behind). Do not wholesale merge older branches. Review discrete missing changes only, preserving `main` as development baseline.

## Read the model documentation before integration

Provider: https://docs.typesafe.ai/ and https://console.typesafe.ai/ . Documented inference endpoint: `POST https://api.typesafe.ai/v1/systemone` (check latest provider docs before implementation). Research version: pin `jev-1.13.0` rather than dynamically resolving `jev-latest`; verify model availability, authorization, response schema, quotas, latency and cost from intended deployment region. JEV has `state` + `questions` with `choice`, `noul`, `score` outputs; read official documentation for each output and their confidence semantics. Provider output confidence must **not** be relabeled as calibrated BTC price-up probability or profitability. Provider warns against treating the model as a high-precision numeric calculator. Feature math, bps, fills, pricing, fee arithmetic, sizing and risk remain deterministic Rust/validated quantitative code.

Read these primary docs and record versions/access date: https://docs.typesafe.ai/api ; https://docs.typesafe.ai/models ; https://docs.typesafe.ai/confidence ; https://developers.binance.com/en/docs/derivatives/portfolio-margin/trade/New-UM-Order ; https://developers.binance.com/en/docs/derivatives/portfolio-margin/trade/Query-UM-Order ; https://developers.binance.com/en/docs/derivatives/usds-margined-futures/websocket-market-streams/How-to-manage-a-local-order-book-correctly . If pages move, locate official replacement and explicitly document changed fields, including `newClientOrderId`, time-in-force/GTX, position mode and reduce-only. Get effective fee rates from the actual account, not outdated promotional rates.

## Architecture: JEV advice NEVER blocks the hot path

```
Binance BTCUSDC public trades + BBO + diff depth
    -> pg-binance local L2 book (snapshot/diff bridge, gap/resync, stale gate)
    -> pg-marketdata normalized events
    -> deterministic Rust microstructure features (OFI, microprice, spread,
       trade imbalance, volatility, inventory, account-margin view)
             |                                   |
             | bounded/coalesced sampling         | uninterrupted hot path
             v                                   v
       JEV async worker                   local quotes, cancels,
       version pin, timeout,              owned exits, risk checks
       rate limit, circuit breaker
             |
       versioned ModelObservation (NO OrderIntent)
             |
       sample-time match + TTL / schema / freshness gate
             |
       existing PolicyEngine -> deterministic fee-aware expected edge
             |
       Risk -> journal-before-dispatch -> OMS -> Binance ExecutionAdapter
             |
       user stream + REST order / fill lookup -> scheduled reconcile
             |
       durable filled quantity, fees, position ownership and metrics
```

Cloud JEV inference must be asynchronous and bounded. Coalesce to latest feature sample, drop stale responses, cap outstanding requests and never pause WS consumption, cancel, reduce-only exit, or reconciliation awaiting HTTP. A model timeout/invalid response prevents **new model-dependent entries**; it cannot disable safe maintenance of an existing strategy-owned position. Ensure a single decision engine owns every strategy definition.

Proposed versioned `ModelObservation` (new additive contract, not implemented yet): `observation_id`, `model_name/version`, `feature_schema/hash`, `question_schema/hash`, instrument, feature event/receive timestamps, request/response monotonic durations, expires_at, normalized choice distribution, provider confidence, validity/reason and provider token/cost metadata. Check probability range/sum and schema. Store model response alongside contemporaneous input for replay, with no look-ahead or replay-time re-querying.

## Binance market and execution work

1. Verify exact BTCUSDC contract `exchangeInfo`, symbol status, price and size filters, minimum notional, account mode/position side, leverage and account permission. Fail startup/ready state if unresolved.
2. Correct official USDⓈ-M book synchronization: buffer diff messages; obtain REST snapshot; bridge update IDs per *futures* documentation; detect gaps, crossed/stale state, reconnects and 24-hour WS disconnect behavior; resync before publishing eligible state. Never invent a sequence from local timestamps.
3. Build isolated `pg-binance` public WS feed and explicit runtime feature flag/capability. Initial milestone is **market-data-only** with no trading credentials or live routing.
4. Add actual PM UM execution adapter: compliant stable client ID, LIMIT with validated GTX post-only support when applicable, exact decimal price/step rounding, reject handling, cancel, query by client ID, open/history/trades, account and positions, plus user-stream events. `POST` timeout/5xx is `Unknown`, not rejection. Resolve stable ID through open AND historical executions before contemplating any new external side effect. Handle partial fills during cancel, duplicate/late fills, market-order exit and one-way/hedge reduce-only semantics. Never rely on unverified API field names.
5. Implement scheduled reconciliation into durable OMS/order records; journal before dispatch with lease/fencing; prove no duplicate exposure across accepted-order/lost-ACK crash window. Exclude unrelated manually owned positions while including all account positions in account-wide margin/risk.
6. Emergency reduce-only flatten must be authenticated, idempotent and pass exchange truth checks. Do not use stale legacy conditional-order fields to determine whether PM UM algo protections exist.

## First strategy experiments, not a claim of alpha

Implement **deterministic baseline**: passive post-only quotes, spread/volatility gates, inventory skew, queue-age-aware cancel/replace, max order count, limits, stale-feed hold and emergency unwind; compare against JEV advisory regime filter on exactly the same market data. JEV can classify `up/down/neutral` or `low/high/unknown adverse selection` on structured *bucketed* microstructure features; no model-generated price/size or direct order endpoints. A maker→taker exit can erase a small expected edge; use actual account fees and venue fills, not assumed zero-fee conditions. Directional taker strategies require empirically validated edge above both fees and adverse-selection/slippage tails.

Replay must be causal (feature available at time t, inference available only after response arrival), with realistic queue uncertainty, partial fills and adverse-selection stress, account-fee schedule and walk-forward splits. Require a deterministic no-JEV benchmark, an ablation, probability calibration on held-out data and latency distributions. Public L2 does not expose exact own order queue priority; report fill-model sensitivity rather than invented certainty.

## Incremental main-branch commit gates (one domain at a time)

- **A — Documentation and contract groundwork:** this document; deterministic Binance-safe client ID helper + tests, without changing global core IDs or enabling live. Review CI after each main commit.
- **B — Public market data only:** WS trades/BBO/L2 snapshot-diff bridge, deterministic gap/reconnect fixtures, clock/freshness metrics, startup capability; no trading credentials.
- **C — Execution boundary:** compliant exchange order contract, order query/filled history/user stream/fees, Unknown recovery, precision/GTX/reduce-only tests. Keep current live startup disabled.
- **D — Durable lifecycle:** continuous fill→OMS reconcile, account/strategy ownership, crash/network/database/fencing fault injection, emergency reduce-only exit and alerting.
- **E — JEV advisory:** version-pinned client, schema validation, bounded inference, response journal, TTL and timeout gates, no model-to-order calls.
- **F — Economic validation:** deterministic baseline vs JEV challenger, causally faithful replay, actual account fee checks, measured p50/p95/p99 end-to-end telemetry and risk-based pilot criteria.
- **G — Operator-approved release only:** read-only connectivity -> shadow -> faithful paper where available -> separately approved tiny allowlisted live canary. Do not silently update production compose, main deployment or existing Binance PM bot.

Hard safety checks throughout: restart while POST accepted/ACK lost; partial fill then cancel; late/duplicate user stream; unknown ownership; stale/gapped L2; JEV 429/timeout/schema drift/stale response; database failure/fencing loss; emergency flatten while feed disconnected; manual/strategy positions in same PM account. All must block unsafe new exposure and preserve a safe, evidence-driven reduce-only path.

Before claiming implementation complete: run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, relevant feature tests, replay and failure injection; capture explicit output and list unverified real-exchange assertions. A successful GitHub commit by itself is not a passing test or production deployment.
