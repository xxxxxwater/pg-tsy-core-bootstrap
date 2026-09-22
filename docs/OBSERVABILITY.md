# Runtime observability API

`pg-core` exposes a read-only runtime evidence plane for operator tooling such as **PG TSY Runtime Console for DeepSeek Harness**.

The central rule is:

> **The API reports evidence, not optimism.** A component that has not proved its state is `PENDING` or `UNKNOWN`; it is never painted `HEALTHY` merely because the process is alive.

## Endpoints

```text
GET /v1/snapshot
GET /v1/events?after=<seq>&limit=<1..200>
```

The default listener is loopback-only:

```text
127.0.0.1:8787
```

Configure it with:

```bash
export PG_OBSERVABILITY_BIND=127.0.0.1:8787
export PG_OBSERVABILITY_STALE_AFTER_MS=10000
export PG_OBSERVABILITY_EVENT_CAPACITY=1024
```

A non-loopback bind is rejected unless `PG_OBSERVABILITY_TOKEN` is set. Clients send it as:

```http
Authorization: Bearer <token>
```

The DeepSeek Harness plugin should keep that token on its Host side (`PG_TSY_RUNTIME_TOKEN`) and proxy the browser over Harness' same-origin authenticated connection.

## Snapshot contract

The response schema is `runtime.snapshot.v1` and contains:

- telemetry capture time and staleness threshold;
- runtime environment, instance id, run mode, build version and uptime;
- derived global safety state and new-exposure gate;
- runtime lease/fencing evidence;
- journal/checkpoint/dispatch evidence;
- reconciliation state;
- market-data feeds;
- venue market-data/execution/reconciliation state;
- strategy inventory;
- positions and ownership;
- OMS order summary including `UNKNOWN` outcomes;
- optional performance evidence;
- exposed operator capabilities;
- startup gates and their exact status.

Unknown evidence is intentionally represented explicitly. For example, a freshly started `pg-core` process that has loaded strategy configuration but has not yet acquired its PostgreSQL lease or attached live venue/reconciliation components will return approximately:

```json
{
  "schema_version": "runtime.snapshot.v1",
  "safety": {
    "state": "SAFE_HOLD",
    "allow_new_exposure": false,
    "reason": "startup gates are not all passed"
  },
  "lease": {
    "required": true,
    "owned": false
  },
  "storage": {
    "journal": "UNKNOWN"
  },
  "reconcile": {
    "status": "UNKNOWN"
  }
}
```

That is a correct state, not an error in the console.

## Event contract

Events are process-local, monotonically sequenced and held in a bounded in-memory ring buffer. `/v1/events?after=N` returns only events with `seq > N`.

Current startup events include runtime configuration loading, strategy inventory loading/missing state, observability listener startup and shutdown requests. Live components should add domain events at their own authoritative transition points, for example:

```text
lease.acquired
lease.heartbeat
lease.lost
feed.healthy
feed.stale
oms.partial_fill
execution.unknown
reconcile.match
reconcile.mismatch
safety.safe_hold
```

The event buffer is an operator stream, not the durable trading journal. It must never be used as evidence that an external order side effect did or did not happen.

## Safety derivation

`RuntimeObservatory` derives the effective state from the evidence it currently holds. `SAFE_HOLD` is forced when any required authority is unresolved, including:

- startup gates not all passed;
- required runtime lease not owned;
- durable journal health not proven;
- unknown order outcomes;
- reconciliation/ownership mismatch;
- required market-data feed health not proven;
- in live mode, execution or venue reconciliation health not proven.

`SHADOW`, `NORMAL` or `DEGRADED` are reachable only after blocking evidence has cleared.

## Integration contract for live components

The observability API owns no trading logic. Existing components keep their authority and report state into a shared `RuntimeObservatory`:

```text
Postgres lease heartbeat  ---> set_lease(...)
Journal/checkpoint path    ---> set_storage(...)
Subscription supervisor   ---> set_configured_feeds(...) / update_snapshot(...)
OMS                        ---> set_orders(...)
Position ownership        ---> set_positions(...)
Reconcile loop             ---> set_reconcile(...)
Runtime halt               ---> set_halted(...)
All components             ---> record_event(...)
```

This separation is deliberate: losing the HTTP server or DeepSeek Harness must not alter order, risk, ownership or recovery behavior.

## Current boundary

The API and shared state carrier are implemented before the complete live orchestration is finished. Therefore the first real snapshot is expected to show which P0 components are still unwired. As the continuous live runtime is completed, those components should update the same snapshot rather than inventing a second monitoring model.
