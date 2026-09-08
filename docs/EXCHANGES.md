# Venue adapters

Core crates do not depend on venue request/response types. Every venue is isolated behind a small adapter crate.

## Hyperliquid

`pg-hyperliquid` pins the official `hyperliquid-dex/hyperliquid-rust-sdk` to a reviewed commit behind the `sdk` Cargo feature. The SDK feature is optional so core CI/replay can run without pulling venue/network dependencies.

Responsibilities of this adapter:

- Info API snapshots and account state;
- websocket subscriptions for trades, book data and user/order events;
- order submission/cancel/modify mapping;
- nonce/signing isolation;
- mapping venue responses into `pg-types`;
- reconnect/resubscribe and snapshot-after-gap behavior.

## Interactive Brokers

Interactive Brokers publishes the official TWS API, but not an official Rust SDK. `pg-ibkr` therefore wraps the maintained community `rust-ibapi` crate behind the `sdk` feature. Do not call it an official IBKR Rust SDK in documentation or incident reports.

`rust-ibapi` 3.x talks to TWS / IB Gateway using the newer protobuf wire protocol and requires a sufficiently recent TWS/IB Gateway server. The adapter boundary lets us replace the community SDK later without changing OMS/risk/strategy code.

IBKR market data is not a public websocket like a crypto venue. The common internal event stream is fed from the TWS/IB Gateway API subscription model.

Responsibilities:

- contract resolution and canonical instrument IDs;
- tick-by-tick / market-data / real-time bar subscriptions;
- account/position/order state;
- order placement/cancel mapping;
- reconnect and subscription recovery;
- mapping TWS notices/errors into typed core events.

## Feature flags

```bash
# Core/offline CI: no venue SDKs required
cargo test --workspace

# Explicitly compile adapter SDK bindings
cargo check -p pg-hyperliquid --features sdk
cargo check -p pg-ibkr --features sdk
```

Real venue integration tests remain opt-in and must never run against a live account by default.
