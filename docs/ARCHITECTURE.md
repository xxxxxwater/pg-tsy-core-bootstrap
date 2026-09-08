# Architecture

## Objective

Build a TSY-like *three-path* quant stack that remains understandable by one human maintainer and AI agents:

- **Factor path**: discover, evaluate and version statistical factors in Python/Polars.
- **ML path**: train/validate short-horizon order-book and order-flow models locally with PyTorch/JAX and robust walk-forward evaluation.
- **HFT/execution path**: deterministic Rust state machines for live market data, online factors, strategy state, risk, OMS, execution, ownership and recovery.

## Hard boundary

The only thing research exports to live trading is a **versioned artifact, frozen parameter set or signal**. Research never owns live orders or positions and does not submit venue requests.

```text
Research Plane                                  Live Plane
--------------                                  ----------
Parquet/S3                                      venue feeds
   |                                                |
Arrow/Polars                                  MarketDataSource
   |                                                |
Factor --------+                              Trade/BBO/L2/Candle
               |                                    |
PyTorch/JAX ---+----> artifact/signal ----> RollingFactorEngine
                                                    |
                                               Signal.v1
                                                    |
                                              StrategyMachine
                                                    |
                                             PositionTarget
                                                    |
                                                RiskEngine
                                                    |
                                                   OMS
                                                    |
                                             ExecutionAdapter
                                              /            \
                                       Hyperliquid         IBKR
                                          cloid          order_ref
                                              \            /
                                                Venue truth
                                                    |
                                           Ack/Fill/Reject
                                                    |
                                     Journal / Reconcile / Checkpoint
                                                    |
                                           Position Ownership
```

## Why a monorepo

For a one-person team, a monorepo makes contract changes, replay fixtures, CI, dependency updates and agent context easier to reason about. Separate repositories are only justified when access control or independent release cadence becomes a real requirement.

## Why not microservices first

Network boundaries create failure modes. The current design prefers:

- in-process Rust channels inside the live core;
- PostgreSQL for durable metadata, fencing, orders, ownership and checkpoints;
- S3/Parquet for bulk/tick data;
- explicit process boundary between research and live trading;
- venue SDKs isolated behind adapter crates.

Introduce gRPC/SHM only for a measured latency/throughput requirement. Introduce Kafka/EKS only when operational evidence justifies the extra failure modes.

## Market-data path

Venue transport is normalized into common events:

```text
venue transport
     |
MarketDataSource
     |
Trade / BBO / L2 / Candle
     |
FeedFreshness + gap semantics
     |
RollingFactorEngine
     |
Signal
```

Sequence numbers are evidence, not decoration. A feed uses `sequence` only when the venue exposes a trustworthy ordering primitive. Otherwise the field remains `None` and the runtime relies on freshness, reconnect and snapshots. It must never fabricate a sequence from a timestamp, exchange label or local counter and then treat it as venue truth.

## Strategy / position path

Strategy code expresses desired exposure through a state machine rather than calling venue APIs directly:

```text
Flat
 |
EnteringLong / EnteringShort
 |
Long / Short
 |
Exiting
 |
Flat
```

Partial fills update strategy-owned quantity incrementally. Ownership is separate from venue position quantity: a venue position may be strategy-owned, manual, mixed/resolved by evidence, or unknown. Manual positions are not silently adopted by a strategy.

## Execution identity

Every persisted `OrderIntent` has deterministic client identity. Venue adapters map that identity onto the strongest stable native/reference field available:

- **Hyperliquid**: persisted intent UUID → `cloid`;
- **IBKR**: `OrderIntent.client_order_id()` → TWS `order_ref`.

This identity is used before replayed submission and after any ambiguous external result.

## Ambiguous external outcome

The critical rule is:

> **Unknown is neither success nor rejection.**

```text
OMS intent
   |
lookup stable identity
   |
submit once
   |
 +-----+----------------+
 |     |                |
ack  reject          timeout/error
 |     |                |
adopt terminal      venue lookup/history
                        /        \
                     found     unresolved
                       |           |
                     adopt       Unknown
                                   |
                             reconcile/hold
                                   |
                              no blind retry
```

Hyperliquid recovery is keyed by `cloid`. IBKR recovery checks open orders, completed orders and executions carrying `order_reference`. A quickly filled order discovered in execution history is proof that the earlier placement was accepted and must not be replaced.

## Position ownership

A venue position is not enough to infer ownership. Ownership is reconstructed from:

- strategy identity;
- stable client order identity;
- journaled intents and fills;
- durable order records;
- venue order/fill history;
- prior reconciled ownership state.

Anything unresolved is `Unknown`. Unknown blocks **new strategy exposure** at the narrowest safe venue+asset scope, while known manual positions remain manageable and are not automatically converted into global SAFE_HOLD.

## Durable state and fencing

The production design uses a single active live writer guarded by PostgreSQL lease/fencing:

```text
instance A -- fencing 41 ----+
                             | durable writes
instance B takeover ---------+--> fencing 42

late write from fencing 41 -> rejected
```

The store tracks lease/fencing, append-only journal metadata, checkpoints, order records, position ownership, reconcile history and command audit. The venue remains authoritative for whether an external side effect actually occurred.

## Recovery authority

Cold-start truth is reconstructed from:

```text
checkpoint
    +
journal after checkpoint
    +
durable order/ownership records
    +
venue open/history/fills/positions
    =
reconciled runtime state
```

A checkpoint cannot turn uncertainty into certainty. If local state and venue truth disagree, the runtime must quarantine the affected scope and reconcile before opening new exposure.

## Journal-before-dispatch requirement

The venue adapters now provide the stable identities needed to recover ambiguous submits, but full unattended-production acceptance additionally requires the live runtime to prove the **journal-before-dispatch** ordering around every exposure-changing request.

The project should not claim system-wide exactly-once execution merely because a venue supports a client order id. The concrete acceptance invariant is:

> after kill-9/network/database faults around submission, recovery can reconstruct the accepted external order/fill and prove that no blind duplicate exposure-increasing request was emitted.

## Failure model

The live core distinguishes:

- known success;
- known rejection;
- unknown external outcome;
- partial fill;
- stale/sequence-gapped market data;
- lost runtime lease/fencing mismatch;
- database/journal unavailable;
- account/order/position reconciliation mismatch;
- unknown ownership.

Unknown and stale states are fail-closed for new exposure. Emergency reduction and manual-position handling must still follow explicit ownership/risk semantics rather than a blanket global freeze.
