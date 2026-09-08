# Operations runbook outline

## Startup gate

Before new exposure is allowed:

1. Journal writable.
2. Database reachable.
3. Venue authentication healthy.
4. Market-data sequence synchronized.
5. Open orders fetched.
6. Positions/balances fetched.
7. Ownership reconciliation complete.
8. Unknown state count is zero or explicitly quarantined.
9. Strategy allowlist loaded.
10. `PG_LIVE_TRADING=true` is explicitly configured.

## Shutdown

A process shutdown is not an order cancellation policy. The configured policy must explicitly choose whether resting orders are canceled, preserved or reduced.

## Incident modes

- `NORMAL`: normal strategy operation.
- `SAFE_HOLD`: no new strategy exposure; reconciliation/reduce-only permitted.
- `HALT`: execution disabled except an explicitly configured emergency path.

Manual positions remain external/manual unless proven strategy-owned.
