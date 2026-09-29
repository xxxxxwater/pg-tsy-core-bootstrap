# Hyperliquid / IBKR paper fault matrix

This matrix is a **deterministic software acceptance gate**, not evidence that an external
Hyperliquid Testnet account or IBKR paper account has been exercised successfully.

The dedicated `paper-fault-matrix` workflow proves the code-level invariants that must
hold before any external-account acceptance run is allowed.

| Fault / invariant | Hyperliquid | IBKR | Gate |
| --- | --- | --- | --- |
| Stable external identity | `cloid` from durable intent | `order_ref` from durable intent | adapter tests |
| Lost submit ACK | recover by stable ID; never second submit | recover by stable ID; never second submit | orchestrator tests |
| Crash after venue accepted / before local ACK | adopt venue order | adopt venue order | orchestrator tests |
| UNKNOWN order state | blocks clean reconcile | blocks clean reconcile | reconcile tests |
| Partial fill drift | SAFE_HOLD | SAFE_HOLD | reconcile tests |
| Native immutable fill ID | Hyperliquid `tid` | IBKR string `ExecId` | execution/store tests |
| Fill ledger replay | exact quantity must equal OMS | exact quantity must equal OMS | Postgres tests |
| Fencing loss | blocks dispatch/cancel | blocks dispatch/cancel | orchestrator tests |
| Reduce-only cross-flat | native guard + core effect | software guard + core effect | adapter/core tests |
| Manual/foreign position | never adopted or flattened | never adopted or flattened | ownership tests |
| Restarted PositionView | rebuilt from fenced fills | rebuilt from fenced fills | pg-core tests |

## External paper acceptance still required

A software-green matrix does not prove venue behavior under real network timing. Before
calling either venue unattended-ready, separately exercise an isolated external account
and retain timestamped evidence for:

1. submit ACK loss / disconnect before ACK;
2. partial fill, fee/commission evidence and restart;
3. cancel-vs-fill race;
4. venue disconnect and reconnect;
5. process kill between venue acceptance and local persistence;
6. manual/foreign position coexistence;
7. owned-only emergency flatten;
8. final account/OMS/fill-ledger equality.

No production or paper-live credential is placed in GitHub Actions.
