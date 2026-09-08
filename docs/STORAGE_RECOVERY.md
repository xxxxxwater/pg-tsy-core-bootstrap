# Storage, fencing and recovery

Production state is split between an append-only journal, compact checkpoints and venue truth.

## PostgreSQL responsibilities

`pg-store` owns four tables:

- `runtime_leases`: single-writer lease and monotonically increasing fencing token;
- `event_journal`: immutable strategy/order/reconcile/control events;
- `checkpoints`: latest compact state plus journal sequence;
- `command_audit`: authenticated operator commands and outcomes.

The market-data lake remains Parquet/S3; PostgreSQL is not the tick-data warehouse.

## Fencing

Before any process is allowed to route real orders it must acquire the `live-core` lease. A different holder can acquire the lease only after expiry. Ownership changes increment `fencing_token`.

Every durable trading write carries the fencing token. A process that cannot renew its lease must enter `SAFE_HOLD` immediately and must not create new exposure. Venue adapters should additionally stamp an instance/strategy/client-order identity where supported so reconciliation can distinguish owners.

Lease TTL should be comfortably larger than the renewal period. Example:

```text
TTL:      15 seconds
renew:     5 seconds
fail hold: first failed renewal or token mismatch
```

Do not use a long lease to hide unreliable connectivity.

## Cold-start recovery

1. connect PostgreSQL;
2. acquire runtime lease and fencing token;
3. load checkpoint;
4. replay journal after checkpoint sequence;
5. connect venue(s);
6. fetch open orders, fills, positions and balances;
7. compare internal replayed state with venue truth;
8. classify every position/order as strategy-owned, manual or unknown;
9. quarantine mismatches/unknowns;
10. resubscribe market data and obtain required snapshots;
11. mark startup gates;
12. only then permit new live exposure.

A checkpoint is an optimization. The journal + venue truth remain authoritative for recovery.

## Kill -9 rule

Assume the process can die after the venue accepted an order but before the local acknowledgement was persisted. Client-order identity and cold-start reconciliation must make this an `Unknown -> Reconcile` path, never an automatic blind retry.
