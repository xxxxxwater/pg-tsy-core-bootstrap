# Venue adapters

Core crates do not depend on venue request/response types. Every venue is isolated behind a small adapter crate and translated into `pg-marketdata` / `pg-execution` contracts.

The adapter boundary has two jobs:

1. normalize venue-specific transport and state into shared types;
2. preserve venue-specific safety semantics instead of pretending every exchange behaves the same way.

## Shared execution rule

Every replayable order intent has a stable client/reference identity. Before a persisted intent can be sent again after restart, the adapter must search venue truth for that identity.

```text
persisted intent
      |
stable client identity
      |
lookup venue truth
   /        \
found      absent
  |          |
adopt     submit once
             |
       ack / reject / ambiguous
                      |
                  reconcile
                  /       \
               found    unresolved
                 |          |
               adopt     Unknown
                           |
                     no blind retry
```

`ExecutionError::Unknown` is intentionally different from `Rejected`: unknown means the system cannot prove whether the external side effect occurred.

## Hyperliquid

`pg-hyperliquid` pins the official `hyperliquid-dex/hyperliquid-rust-sdk` to a reviewed commit behind the `sdk` Cargo feature. The SDK feature is optional so core replay/CI can run without venue/network dependencies.

### Market data

The adapter maps official SDK websocket streams into shared market events:

- trades;
- best bid/ask;
- L2 book;
- candles.

Reconnect/resubscribe is isolated inside the venue boundary. Feed sequence/freshness semantics are kept explicit rather than inferred from unrelated fields.

### Execution

Hyperliquid provides a native client order id (`cloid`). The adapter uses the persisted `OrderIntent` UUID as that stable venue identity.

Submission semantics:

1. query by `cloid` before POST;
2. if already present, adopt the existing order/fill state and do not POST again;
3. otherwise submit once with that same `cloid`;
4. a clear venue rejection returns `Rejected`;
5. a transport/protocol ambiguity triggers another `cloid` lookup;
6. if venue state proves the order exists, adopt it;
7. if still unresolved, return `ExecutionError::Unknown` and require reconciliation before any replacement order.

Read-side recovery maps open/historical order information and account positions into `VenueOrderSnapshot` / `VenuePositionSnapshot`. Partial-fill state is derived from requested/remaining size rather than assuming an acknowledged order is unfilled or fully filled.

Hyperliquid reduce-only is mapped to the venue-native order field where supported by the SDK.

## Interactive Brokers

Interactive Brokers publishes the official TWS API, but not an official Rust SDK. `pg-ibkr` wraps the maintained community `ibapi` crate (**4.0.1** at the current integration boundary) behind the `sdk` feature. Do not call it an official IBKR Rust SDK in documentation or incident reports.

IBKR market data is not a public crypto-style websocket. The adapter connects to **TWS / IB Gateway** and normalizes its subscription model into the common event stream.

### Market data

Current mapped feeds include:

- tick-by-tick trades;
- tick-by-tick bid/ask;
- market depth;
- 5-second realtime bars.

IBKR does not expose a trustworthy monotonic sequence for the tick-by-tick path used here, so `MarketEvent.sequence` remains `None`. Never synthesize a sequence from exchange names, timestamps or local counters and then treat it as venue gap evidence.

### Execution identity and recovery

IBKR uses the order's `order_ref` as the stable PG client identity. `OrderIntent.client_order_id()` is written into `order_ref`.

Before a replayed submit, and after an ambiguous placement result, the adapter searches in this order:

1. open orders by `order_ref`;
2. completed orders by `order_ref`;
3. execution reports by `execution.order_reference`.

The third step matters for fast market orders which may already have left the open-order set by the time recovery starts. Finding an execution is proof that the previous placement reached IBKR and therefore a replacement must not be emitted.

An unresolved timeout/connection failure becomes `ExecutionError::Unknown`, not `Rejected`. Cancellation ambiguity likewise requires reconciliation before retry.

### Reduce-only caveat

For ordinary stock orders this adapter does **not** claim a venue-native atomic reduce-only guarantee.

Default behavior is to reject `ExposureEffect::ReduceOnly`. If `allow_software_reduce_only` is explicitly enabled, the adapter immediately refreshes the account position and rejects:

- a side that would increase the position;
- quantity larger than the reconciled absolute position;
- any quantity that could cross through flat into the opposite exposure.

This is a software guard with a timing window, not a substitute for a venue-native reduce-only primitive.

## Binance Portfolio Margin

`pg-binance` remains the Binance/Portfolio Margin venue boundary. The production execution/recovery loop is not yet at the same completion level as Hyperliquid and IBKR. Do not infer live readiness from the existence of the adapter crate.

Binance PM remains part of the P0 production-hardening backlog: user stream/account truth, client-order id recovery, reconcile, reduce-only emergency path and failure-injection acceptance must be completed before canary use.

## CI / feature flags

```bash
cd rust

# Core/offline path
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Venue SDK execution/recovery paths
cargo test -p pg-hyperliquid --features sdk
cargo test -p pg-ibkr --features sdk
cargo clippy -p pg-hyperliquid -p pg-ibkr --all-targets --features sdk -- -D warnings

# Telegram control transport
cargo check -p pg-control --features telegram
```

These CI tests compile and unit-test the venue bindings without intentionally submitting real orders. Real-account tests and failure injection must remain explicit opt-in procedures and must never run against production credentials by default.
