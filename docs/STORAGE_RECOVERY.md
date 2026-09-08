# Storage, fencing and recovery

Production state is reconstructed from three sources: **durable local intent/history, compact checkpoints, and venue truth**. A checkpoint is an optimization; it is never allowed to erase uncertainty about an external side effect.

## PostgreSQL responsibilities

`pg-store` currently models the following durable responsibilities:

- `runtime_leases`: single-writer lease and monotonically increasing fencing token;
- `event_journal`: append-only strategy/order/reconcile/control events;
- `checkpoints`: latest compact state plus journal sequence;
- `command_audit`: authenticated operator commands and outcomes;
- `order_records`: durable order identity, requested/filled quantity and lifecycle state;
- `position_ownership`: strategy/manual/unknown ownership state by venue and asset;
- `reconcile_runs`: reconciliation results and audit history.

The market-data lake remains Parquet/S3; PostgreSQL is not the tick-data warehouse.

## Fencing

Before any process is allowed to route real orders it must acquire the `live-core` lease. A different holder can acquire the lease only after expiry. Ownership changes increment `fencing_token`.

Every durable trading write must carry the active fencing token. A process that cannot renew its lease must stop creating new exposure and enter the configured fail-closed state. An old process which wakes after another instance has taken leadership must be unable to overwrite state written by the newer fencing token.

Lease TTL should be comfortably larger than the renewal period. Example:

```text
TTL:      15 seconds
renew:     5 seconds
fail hold: first failed renewal or token mismatch
```

Do not use a long lease to hide unreliable connectivity.

## Ambiguous-submit rule

The project does **not** claim that a networked exchange POST can be made magically exactly-once. The production objective is narrower and testable:

> never convert an uncertain first submission into blind duplicate exposure.

The intended state path is:

```text
persist intent / identity
        |
lookup stable venue identity
        |
submit once
        |
   +----+-------------------+
   |                        |
known reject            known/possible accept
                            |
                  ack arrives? ---- yes ---> persist/adopt
                            |
                            no
                            v
                     outcome ambiguous
                            |
                      query venue truth
                       /          \
                    found       unresolved
                      |             |
                    adopt        Unknown
                                    |
                         SAFE_HOLD / reconcile
                                    |
                              NO blind retry
```

A caller must distinguish:

- **Rejected** — there is positive evidence that the venue did not accept the order;
- **Unknown** — acceptance cannot be proven either way;
- **Recovered existing order** — venue truth proves the original side effect already exists and the local runtime must adopt it rather than resubmit.

## Venue identities

### Hyperliquid

The persisted `OrderIntent` UUID is used directly as Hyperliquid `cloid`.

Recovery checks `cloid` before posting and again after ambiguous transport/protocol failure. If the order is found, the existing venue order/fill is adopted. If lookup is still inconclusive, the adapter returns `ExecutionError::Unknown` and replacement submission is forbidden until later reconciliation resolves the state.

### Interactive Brokers

`OrderIntent.client_order_id()` is written into IBKR `order_ref`.

Recovery searches:

1. open orders;
2. completed orders;
3. execution reports carrying `order_reference`.

The execution-report fallback is important because a quickly filled market order can disappear from open orders before the recovery query runs. Finding such an execution is proof of prior acceptance and therefore proof that the runtime must not submit a replacement.

## Journal-before-dispatch boundary

Adapter-level idempotency/recovery does **not** remove the need for durable orchestration above the adapter.

Before unattended live acceptance, the live runtime must prove end-to-end that the intended order identity and dispatch state are durably recorded before entering a path where the venue may accept the order. The current venue adapters provide the stable identity and recovery behavior required by that protocol, but the full runtime-level journal-before-dispatch invariant and crash-window failure injection remain part of P0 hardening.

Do not document the system as exactly-once until that entire protocol is implemented and demonstrated under kill-9/network/database faults. Even then, describe the concrete invariant (no blind duplicate exposure after ambiguous dispatch) rather than relying on the phrase alone.

## Cold-start recovery

1. connect PostgreSQL;
2. acquire runtime lease and fencing token;
3. load checkpoint;
4. replay journal after checkpoint sequence;
5. load durable order records and ownership state;
6. connect venue(s);
7. fetch open orders, completed/history/fills as supported, positions and balances;
8. compare internal replayed state with venue truth using stable client identity;
9. classify every position/order as strategy-owned, manual or unknown;
10. quarantine mismatches/unknowns at the narrowest safe venue+asset scope;
11. resubscribe market data and obtain required snapshots/freshness;
12. record reconciliation result and checkpoint;
13. mark startup gates;
14. only then permit new live exposure.

Manual positions are not automatically errors. They remain manual unless ownership evidence explicitly links them to a strategy. Unknown ownership is different: it means the system lacks enough evidence to act safely.

## Kill -9 acceptance case

The critical failure test is:

```text
local intent exists
      |
venue accepts order
      |
process dies before local ACK persistence
      |
restart
      |
replay durable identity
      |
query venue by cloid/order_ref/history/executions
      |
find original order/fill
      |
adopt it
      |
prove no second exposure-increasing POST was emitted
```

The symmetric case must also be tested: if the process dies before the venue accepted anything, recovery must not guess. It may resubmit only after the recovery protocol positively establishes that doing so is safe under the venue's identity semantics.
