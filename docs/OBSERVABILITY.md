# Observability architecture and integration status (2026-09-29)

The observability plane is now **wired into `pg-core --serve` as an opt-in operator API**. It remains read-only and has no path that can submit, cancel, flatten, clear `SAFE_HOLD`, or change risk authority.

## Runtime surfaces

| Surface | Status | Security / meaning |
| --- | --- | --- |
| `GET /healthz` | Always part of `pg-core` health listener | Process/runtime health summary |
| `GET /readyz` | Always part of `pg-core` health listener | Startup/readiness gate summary |
| `GET /metrics` | Always part of `pg-core` health listener | Prometheus text metrics |
| `POST /admin/reload` | Wired | Default-deny; requires a valid 32-512 byte printable `PG_ADMIN_TOKEN` Bearer token |
| `GET /v1/snapshot` | Opt-in integrated | Start with `PG_OBSERVABILITY_ENABLED=true` |
| `GET /v1/events?after=N&limit=...` | Opt-in integrated | Same listener/auth boundary as snapshot |

The observability listener defaults to `127.0.0.1:8787`. A non-loopback bind requires `PG_OBSERVABILITY_TOKEN`, which must be 32-512 printable ASCII bytes. Authentication compares the complete Bearer credential.

## Configuration

```bash
PG_OBSERVABILITY_ENABLED=false
PG_OBSERVABILITY_BIND=127.0.0.1:8787
PG_OBSERVABILITY_TOKEN=
PG_OBSERVABILITY_STALE_AFTER_MS=10000
PG_OBSERVABILITY_EVENT_CAPACITY=1024
```

The feature is disabled by default to preserve existing deployment behavior and avoid silently opening a second port during an upgrade.

## Authoritative fields wired today

The integrated bridge updates the observatory from actual daemon state for required startup gates, lease ownership, loaded strategy inventory (including successful reloads), open-order count, reconciliation health, and durable journal availability established at startup.

These fields participate in conservative safety recomputation. Missing or failed startup/reconcile/lease evidence remains fail-closed.

## Still incomplete

Detailed per-feed age/status, per-venue latency/execution/reconcile rows, full position ownership rows, recent order detail, checkpoint/tail sequence numbers, and PnL/exposure performance are not yet authoritative producers. They must remain unknown/empty rather than being synthesized from process health.

The next acceptance step is an integration test that launches the real `pg-core` daemon against isolated Postgres, verifies `/v1/snapshot` and `/v1/events`, exercises Bearer denial, forces lease/feed/reconcile failures, and proves the API never unlocks exposure.

## Safety boundary

Observability is evidence only. Risk, OMS, durable dispatch and reconciliation remain the authority. UI failure must not stop the trading safety loop, and UI success must never promote a runtime out of `SAFE_HOLD`.
