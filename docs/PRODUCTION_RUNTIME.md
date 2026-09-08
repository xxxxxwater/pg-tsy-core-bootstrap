# Production runtime contract

The live binary has three explicit execution modes. They are not aliases.

| Mode | Real market data | Real account reconcile | Sends real orders |
| --- | --- | --- | --- |
| `shadow` | yes | optional | never |
| `paper` | yes | optional | never; simulated execution only |
| `live` | yes | required | only after every startup gate passes |

`PG_RUN_MODE=live` is insufficient by itself. `PG_LIVE_TRADING=true` is a second deliberate key and production startup also requires fencing/lease ownership, fresh market data, venue/account truth, ownership reconciliation and a loaded strategy allowlist.

## Startup gates

The Rust `pg-runtime` crate makes the runbook executable. Live mode requires:

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

Any failed/pending required gate blocks new real exposure.

## Runtime incident modes

- `NORMAL`: strategy entries/exits can proceed subject to risk.
- `SAFE_HOLD`: no new strategy exposure; reconciliation and reduce-only actions continue.
- `HALT`: normal execution is disabled; an idempotent emergency flatten path may be configured separately.

## Feed freshness

Every normalized event has event/receive timestamps. Each subscription owns a freshness guard. If `now - last_recv` exceeds `PG_MAX_MARKET_STALENESS_MS`, strategies depending on that feed enter a non-entry state until a resync/snapshot completes.

## Shutdown policy

`PG_SHUTDOWN_POLICY` is one of:

- `preserve`: leave resting orders unchanged;
- `cancel_resting`: cancel strategy-owned resting orders, keep positions;
- `flatten_owned`: cancel then reduce-only flatten strategy-owned exposure.

Shutdown policy never applies to manual/unowned positions.
