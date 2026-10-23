# PureGamma RSS V4 × PG-TSY × Coinbase LIVE

**Design checkpoint: 2026-10-23. Status: read-only bridge implemented;
Coinbase LIVE execution not implemented; NO-GO for real orders.**

## Observed runtime versus claims

On the Oregon `instabox-pg-test` server (approximately 4 vCPU, 8 GiB and
20 GiB persistent storage), the read-only inspection showed PureGamma RSS V4
at 98 events and 2 sent Telegram messages, and TSY `/healthz` ready with
8/8 connected market-data feeds, running `shadow`. This is a point-in-time
observation, NOT exchange trading acceptance, continuous uptime or
evidence of profitable orders.

A Coinbase SDK preflight was present in the InstaBox checkout but a
Coinbase venue was not registered in the reviewed GitHub `main`
`signal.v1` / OMS execution contracts. API key values were not read.

## Redesign: data, monitoring and order boundaries

~~~text
Official/market RSS + RSSHub + WatcherGuru
       |
       v
PureGamma V4: raw English event, provenance, dedup, priority
       +----> LibreTranslate -> Chinese Telegram headlines (display only)
       |
       v
SQLite events + articles [READ ONLY for bridge]
       |
       +----> Deterministic official-origin review candidate
       |      (REVIEW_REQUIRED; NO BUY/SELL/SIZE)
       |
       +----> Independent TSY + RSS heartbeat -> Telegram outbox
                                      |
                    future, NOT yet implemented:
                                      v
Native Rust Coinbase venue -> pg-risk + PositionView
       -> durable fenced intent -> OMS -> Coinbase order adapter
       -> fills/fees/protection/restart reconciliation
~~~

The candidate rule requires `articles.source = sec_press`, trust A, P0/P1,
fresh publication (120 s), and an affirmative original English spot Bitcoin
ETF approval title. Proposals, stale headlines, denials, rumours, and
community posts are rejected. Headline semantics alone **do not establish
market direction or alpha**. No trading order can be constructed from the
`ReviewCandidate` type.

## Mandatory Coinbase LIVE gates

| Boundary | Required evidence |
| --- | --- |
| Native venue | Versioned Coinbase asset/venue contract and registered Rust adapter |
| Account identity | Dedicated portfolio UUID; trade-only, non-transfer key verified |
| Ownership | Manual existing balances isolated from strategy-owned orders |
| Market checks | Product trading status, fresh order book, lot/tick size, spreads |
| Risk | Approval from `pg-risk`; position, per-order and daily aggregate caps |
| Journal | Fenced writer, durable pre-dispatch intent, deterministic client order ID |
| OMS | Partial fills, cancels, fee entries, idempotent ownership model |
| Protective orders | Post-fill proof of active stop/TP, with failure and gap handling |
| Uncertainty | Timeout/unknown ACK -> read-side recovery and SAFE_HOLD |
| Release | Fault-injection replay, live-account supervision, signed acceptance |

None of these gates may be inferred from a healthy Docker container, a
successful API authentication, a Coinbase order preview, or a test Telegram
message. There is **no paper requirement added to the integration**; real
trading remains blocked until the production native path is proven.

## Telegram heartbeat contract

The independent monitor checks RSS status and TSY health once per minute
(default), warns after three consecutive failures, emits an hourly
heartbeat and reports recovery. Notification state is held in a local
SQLite outbox, with rate-limit-aware retry. Telegram secrets are mounted
from the existing file read-only; **Coinbase keys are not mounted**.

RSS V4's original source event pipeline and bilingual news delivery remain
unchanged. This monitor only adds TSY/RSS health and a conservatively
filtered review alert; it never overrides the RSS priority or source list.

**Host outage caveat:** if InstaBox auto-suspends, an internal monitor also
stops. A separate off-host uptime probe is necessary for outage alerts.

## Install / test / rollback

Follow [the integration runbook](../integrations/puregamma_rss/README.md).
This Compose file does not restart or edit the existing RSSHub / TSY services.
It requires read-only access to the current RSS SQLite schema and
loopback health endpoints.

~~~sh
python3 -m unittest discover -s integrations/puregamma_rss/tests -v
~~~

Tests use mock health responses and temporary SQLite files. They do not
validate live Coinbase execution. Remove the standalone monitoring service
with `docker compose -f compose.monitoring.yml down` to roll back, leaving
the original processes untouched.

## Next real-work milestones

1. Add typed Coinbase `MarketDataSource` / asset keys and versioned contracts
   with Python/Rust compatibility tests.
2. Implement Coinbase `ExecutionAdapter` inside Rust TSY's fenced OMS,
   preserving risk and ownership checks before exchange side effects.
3. Add order-read history, fill/fee/restart reconciliation and explicit
   native protection verification, including network ambiguity matrix.
4. Prove real-account isolated acceptance and approved sizing/kill switch.
5. Add off-host InstaBox liveness checks and verify host auto-suspend status.

Related: [ADR 0003](adr/0003-rss-events-no-order-authority.md),
[Architecture](ARCHITECTURE.md), [Exchanges](EXCHANGES.md),
[Release readiness](RELEASE_READINESS.md), [AGENTS.md](../AGENTS.md).
