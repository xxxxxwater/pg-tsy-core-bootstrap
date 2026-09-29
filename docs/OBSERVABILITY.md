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

The integrated bridge updates the observatory from actual daemon state for required startup gates, lease ownership and the active fencing token, loaded strategy inventory (including successful reloads), open-order count, reconciliation health, durable journal availability established at startup, and each configured market-data feed's state plus receive-age derived from `SubscriptionSupervisor`.

These fields participate in conservative safety recomputation. Missing or failed startup/reconcile/lease evidence remains fail-closed.

## Still incomplete

Per-venue execution/latency rows, full position ownership rows, recent order detail, checkpoint/tail sequence numbers, and PnL/exposure performance are not yet authoritative producers. They must remain unknown/empty rather than being synthesized from process health.

`.github/workflows/daemon-observability.yml` now launches a real shadow `pg-core --serve` process against isolated PostgreSQL, feeds it a deterministic normalized JSONL stream, waits for `/readyz`, proves all required feeds are connected, proves unauthenticated snapshot access is rejected, verifies the authenticated `runtime.snapshot.v1` payload (including fencing token and healthy feed rows), checks `/v1/events`, and shuts the daemon down with SIGINT. The daemon workflow also runs deterministic **stale-feed** and **lease-theft** fault injection. The stale-feed case requires READY -> `/readyz 503` -> snapshot `SAFE_HOLD` with `allow_new_exposure=false`. The lease case starts with a healthy owned lease, mutates the PostgreSQL lease row to a different holder with a higher fencing token, then requires the old daemon heartbeat to detect fencing loss and terminate non-zero. Remaining process-level fault injection is reconciliation drift on the same durable reconcile path used by real venues.

## Safety boundary

Observability is evidence only. Risk, OMS, durable dispatch and reconciliation remain the authority. UI failure must not stop the trading safety loop, and UI success must never promote a runtime out of `SAFE_HOLD`.
