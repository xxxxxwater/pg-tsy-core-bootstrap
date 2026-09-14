# Production runtime contract

The live binary has three explicit execution modes. They are not aliases.

| Mode | Real market data | Real account reconcile | Sends real orders |
| --- | --- | --- | --- |
| `shadow` | yes | optional | never |
| `paper` | yes | optional | never; simulated execution only |
| `live` | yes | required | only after every startup gate passes |

`PG_RUN_MODE=live` is insufficient by itself. `PG_LIVE_TRADING=true` is a second deliberate key and production startup also requires fencing/lease ownership, fresh market data, venue/account truth, ownership reconciliation and a loaded strategy allowlist.

**The current daemon is shadow-only.** `pg-core --serve` refuses `paper` and `live`
outright, and refuses any configuration where `routes_to_real_venue()` is true. `paper`
and `live` rows above describe the intended contract, not a shipped path.

## Execution path

```text
strategy decision (legacy score machine OR portable rule graph, never both)
      |
pg_risk::evaluate_order
      |
DurableExecution::dispatch
      |  assert lease/fencing
      |  save OrderRecord + append order.intent.persisted
      |  OMS SubmitRequested + append order.dispatch.started
      |  assert lease/fencing again
      v
AdapterRegistry -> venue adapter
      |
ack  -> OMS accept, persisted + order.dispatch.acknowledged
unknown/transport -> OMS LostState, persisted + order.dispatch.unknown
reject -> OMS Rejected, persisted + order.dispatch.rejected
```

The journal write happens **before** the adapter call, so a crash after that point can
recover by stable client order id instead of inventing a new intent. `Unknown` is
distinct from `Rejected`: it means the runtime cannot prove whether the external side
effect occurred.

Which engine dispatches is decided per definition, not per tick. A definition whose
compiled rule graph is defined (`PolicyEngine::is_defined`, i.e. it declares at least
one entry or exit rule) is owned by the portable policy path, and the legacy score
machine's `Submit` is suppressed. A definition with only `[automation]` keeps the legacy
path.

## Shadow venue

`PG_RUN_MODE=shadow` registers an in-process `ShadowExecutionAdapter` for Hyperliquid,
IBKR and Binance PM through the same `AdapterRegistry` the durable path uses. Nothing in
this configuration opens a venue connection for execution.

`PG_SHADOW_FILL_MODE` selects venue behaviour:

- `rest` (default): acknowledge the order and leave it resting; nothing fills and no position is created;
- `immediate` (also `immediate_fill`): fill the whole order on acknowledgement, using the limit price or the latest mark.

Both modes acknowledge only. Reduce-only shadow orders are rejected unless they strictly
shrink an existing opposite-signed simulated position, mirroring the IBKR software guard.

The simulated book is marked from every normalized market event (last trade, BBO mid,
book mid or candle close). The daemon rebuilds a real `PositionView` from it — net
quantity, average entry price, filled entries, unrealized return and peak return — and
passes that view into the policy graph, so exit rules see the simulated position rather
than a constant flat view.

The durable `OrderRecord` is written at submit time and on acknowledgement. It is **not**
updated from venue fills yet: continuous reconciliation is the missing link (see
`docs/STATUS.md`).

## Startup gates

The Rust `pg-runtime` crate makes the runbook executable. Live mode requires all 11:

1. journal writable;
2. database reachable;
3. runtime lease/fencing acquired;
4. venue authentication healthy;
5. market-data stream synchronized and fresh;
6. open orders loaded;
7. positions/balances loaded;
8. ownership reconciliation complete;
9. no unresolved unknown state;
10. strategy allowlist loaded;
11. explicit live-trading key enabled.

Every gate is now driven by a real check instead of remaining `Pending`:

- `DatabaseReachable`, `RuntimeLeaseAcquired` and `JournalWritable` are driven by store connect/migrate, lease acquisition and the boot journal event;
- `StrategyAllowlistLoaded` is driven by the loaded strategy set;
- `VenueAuthenticated` fails when a derived feed's venue has no registered execution adapter;
- `OpenOrdersLoaded`, `PositionsLoaded` and `OwnershipReconciled` are set from the per-venue execution snapshot; a snapshot load failure propagates as a startup error rather than a failed gate, and ownership is unambiguous by construction inside the simulated venue;
- `UnknownStateClear` fails when a persisted order is still `Unknown`;
- `LiveTradingExplicitlyEnabled` mirrors `PG_LIVE_TRADING`;
- `MarketDataSynchronized` is re-evaluated on the one-second heartbeat tick and fails while any derived feed is disconnected.

Shadow mode requires a subset (journal, database, lease, market data, strategy
allowlist); `ready` is recomputed from that checklist each tick. Any failed/pending
required gate blocks new real exposure.

## Health and control listener

`PG_HEALTH_ADDR` (default `0.0.0.0:8080`) serves:

| Endpoint | Method | Returns |
| --- | --- | --- |
| `/healthz` | GET | process liveness; 503 when the process is unhealthy |
| `/readyz` | GET | readiness from the startup checklist; 503 while not ready |
| `/metrics` | GET | Prometheus text exposition |
| `/admin/reload` | POST | accepts a strategy reload request (202); 405 for other methods |

The snapshot exposes `open_orders` (orders the runtime believes are resting),
`orders_journaled_total` (orders that reached the durable execution path) and
`blocking_gates` (the gates still pending or failed for the active run mode, captured
when the runtime starts). Metrics include `pg_ready`, `pg_runtime_lease_healthy`,
`pg_market_feeds_connected`, `pg_market_events_total`, `pg_policy_decisions_total`,
`pg_open_orders`, `pg_orders_journaled_total` and `pg_startup_gates_blocking`.

This listener is an operator surface, not a trading surface. Its only command is
"re-validate strategy definitions"; it can never submit, cancel or flatten anything
directly. A rejected reload leaves the running strategy set unchanged because validation
happens before any swap.

## Runtime incident modes

- `NORMAL`: strategy entries/exits can proceed subject to risk.
- `SAFE_HOLD`: no new strategy exposure; reconciliation and reduce-only actions continue.
- `HALT`: normal execution is disabled; an idempotent emergency flatten path may be configured separately.

`pg-runtime` declares `IncidentMode` with these three variants, but the daemon does not
yet select an incident mode at runtime. What exists today is the strategy-level
`StrategyPhase::SafeHold`, which holds entries while position management continues, and
the reconciliation `SAFE_HOLD` ownership verdict.

## Feed freshness

Every normalized event has event/receive timestamps. Each subscription owns a freshness guard. If `now - last_recv` exceeds `PG_MAX_MARKET_STALENESS_MS`, strategies depending on that feed enter a non-entry state until a resync/snapshot completes.

A market-data subscription that fails permanently (after `PG_MARKET_MAX_RECONNECTS`,
default 50 attempts with exponential backoff capped at 30s) marks the runtime unready and
terminates the daemon rather than running on stale data silently.

## Shutdown policy

`PG_SHUTDOWN_POLICY` is one of:

- `preserve`: leave resting orders unchanged;
- `cancel_resting`: cancel strategy-owned resting orders, keep positions;
- `flatten_owned`: cancel then reduce-only flatten strategy-owned exposure.

Shutdown policy never applies to manual/unowned positions.

It is applied when the daemon exits for any reason — `ctrl-c`, a fatal feed error or a
lost lease — and it cancels resting orders before flattening, so a stale resting order
cannot fill while the flatten is in flight. Flatten intents carry
`strategy_id = "shutdown:flatten_owned"` and `ExposureEffect::ReduceOnly`, and travel the
same risk/journal/execution path as strategy orders. A shutdown error is logged as a
warning after the exit reason has already been determined; it does not mask the cause of
the shutdown.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `PG_RUN_MODE` | `shadow` | run mode; `--serve` accepts shadow only |
| `PG_LIVE_TRADING` | `false` | second deliberate key for live |
| `PG_SHUTDOWN_POLICY` | `cancel_resting` | shutdown behaviour |
| `PG_SHADOW_FILL_MODE` | `rest` | simulated venue fill behaviour |
| `PG_HEALTH_ADDR` | `0.0.0.0:8080` | health/control listener |
| `PG_MAX_MARKET_STALENESS_MS` | `3000` | feed freshness budget |
| `PG_LEASE_TTL_SECONDS` | `15` | runtime lease TTL (minimum 5) |
| `PG_MARKET_MAX_RECONNECTS` | `50` | reconnect attempts before a feed is fatal |
| `PG_STRATEGY_DIR` | discovered | strategy definition directory |
| `PG_DATABASE_URL` | required | PostgreSQL connection string |

Venue-specific market-data variables are listed in `docs/EXCHANGES.md`.
