# ADR 0003 — PureGamma RSS events cannot authorize Coinbase orders

**Status:** Accepted for read-only event review and monitoring; Coinbase LIVE
remains NO-GO. **Date:** 2026-10-23.

## Context

The real InstaBox deployment contains PureGamma RSS V4 and a PG-TSY Rust daemon.
RSS V4 has an event database with provenance, deduplication, priority and
bilingual Telegram delivery. The TSY daemon already has real adapters for
Hyperliquid and IBKR, not registered Coinbase execution.

The earlier experimental direct Python Coinbase sidecar could bypass
`pg-risk` and durable order fencing. That would conflict with the repository
`AGENTS.md` invariants and could permit repeated live exposure after
ambiguous order acknowledgements.

## Decision

1. Treat RSS/Telegram/chat content as untrusted data even for official feeds.
   Filter by original source article provenance, not translated titles.
2. Consume RSS databases read-only; the candidate output has no order side,
   size or authorization, and never calls exchange APIs.
3. Only future native TSY risk-approved, journaled, ownership-checked Coinbase
   execution may submit live orders.
4. Every new exposure needs durable pre-dispatch intent, deterministic client
   ID, fencing, fee and quantity precision, fills, native protection checks,
   post-restart reconciliation and an explicit emergency plan.
5. Unknown venue outcome remains UNKNOWN/SAFE_HOLD, never a blind retry.
6. Independent Telegram heartbeat stores notification retries separately
   and has **no access to exchange keys**.

## Consequences

No Coinbase LIVE orders are produced by this integration. Separate
versioned contract changes, adapter registration, real-portfolio testing and
release approval are necessary. Failure to prove a stop order's actual active
state after entry is a NO-GO even if order preview succeeds.

An on-host heartbeat cannot detect its own host suspension; external monitoring
is required for server-down detection. The existing Hyperliquid/IBKR runtime
and `PG_RUN_MODE` are unchanged.

## Rejected

- Direct `create_order` calls from a Python RSS consumer.
- Reusing IBKR/Hyperliquid LIVE routing for Coinbase.
- Trading on Chinese translations, AI summaries or group rumours.
- Treating an API timeout as proof of rejection.
