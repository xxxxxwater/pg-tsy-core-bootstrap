# Binance PM isolated read-only evidence boundary

**Deployment / canary / real-order permission: BLOCKED.** This document covers diagnostic tools, not a production strategy runner. The existing PM/Freqtrade service and all manual positions are out of scope.

## What the tools actually do

- `cargo run -p pg-binance --bin pm_user_probe`: reads Portfolio Margin private User Stream events. It is bounded to three sessions; every session start and exit emits `ReconcileRequired`. Only aggregate counts are printed. No order/position/OMS mutation is performed. Reconnect does not establish REST reconciliation.
- `cargo run -p pg-binance --bin pm_history_probe`: makes bounded, signed **GET** requests to PM UM `userTrades` using `BinanceRestClient` constructed with order submission disabled. It requires a nonzero `fromId` supplied by an operator, verifies forward cursor consistency, stops on a terminal short page, refuses missing pages after 32 pages, and prints aggregate counts only. It deliberately does **not** record trades, persist a cursor or update OMS. A short page covers only the interval after the supplied anchor; it cannot establish older history completeness.
- Neither command proves API permissions are read-only, account segregation, trade ownership, exchange order snapshots, account positions, pricing, realized returns or a deployment go/no-go decision. These must be established independently. Neither program is called by standard CI with exchange credentials.

## Isolated history diagnostic prerequisite

An operator must independently verify the isolated account, API key permissions, BTCUSDC/one-way mode, the provenance of the initial trade-ID anchor and the absence of production or manual-position access. Provision the key and secret through an approved secret store **outside the repository, command history and CI logs**. Do not paste credentials into ChatGPT or put them in an issue. Set the following in the isolated host's secret-managed process environment:

```text
PG_PM_HISTORY_PROBE_APPROVAL=APPROVE_ISOLATED_READ_ONLY_HISTORY_PROBE
PG_RUN_MODE=shadow
PG_LIVE_TRADING=false
PG_ISOLATED_ACCOUNT_SCOPE=<nonsecret segregated scope label>
PG_BINANCE_PM_API_KEY=<secret from secret store>
PG_BINANCE_PM_API_SECRET=<secret from secret store>
PG_PM_HISTORY_FROM_ID=<independently established positive trade id>
```

From `rust/`, execute `cargo run -p pg-binance --bin pm_history_probe`. A successful diagnostic prints `anchor_operator_verified=false`, `oms_updates=0`, `order_posts=0`, `cursor_persisted=false` and `safe_hold_required=true`. Those values are intentional: the operator's selected anchor is *not* independently verified by the executable. An invalid page, unsupported account contract, API error or exhausted page budget returns a nonzero status. These commands are documentation; no actual signed PM session has been performed here.

## Integration boundary still missing

`pg-binance::history_reconcile::verify_trade_history` can validate caller-supplied, bounded pages against independently authenticated order snapshots and durable ownership. `pg-store::fill_ledger::settlement::settle_complete_order_history` can atomically update immutable fill records, OMS cumulative quantity/state and a journal in PostgreSQL under a fenced lease. These are **separate pieces**, not yet a proven daemon pipeline.

Do not admit exposure until the real isolated runtime persistently tracks an authenticated history anchor and watermark, fetches complete pages, verifies every trade's ownership/fee, settles fills plus cursor in one fenced transaction, reconciles the authoritative order **and** exchange position, and demonstrates restart, late-fill, manual-position, lost-ACK, stale-lease and emergency-exit tests. In particular, the current settlement does not mutate exchange-derived positions or persist a history cursor; never interpret its receipt as permission to release `SAFE_HOLD`.

Do not promote Jev from Challenger based on synthetic markouts, a passed code CI, or a model-supplied confidence number. Independently sourced fills, fees, latency and forward walk-forward measurements plus human authorization are necessary for a separate tiny canary review.
