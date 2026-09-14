# Replay fixtures

Synthetic, hand-written inputs for the offline replay paths. They are not captured
venue data and contain no credentials.

| File | Consumed by | Format |
| --- | --- | --- |
| `hype.jsonl` | `pg-core --replay-market-events` (`make strategy-replay`) | normalized `MarketEvent` JSONL |
| `policy_features.jsonl` | `pg-core --replay-policy-features` (`make policy-replay`) | `PolicyReplayFrame` JSONL |
| `strategies/hype_policy.toml` | both targets via `STRATEGY_DIR` | `strategy.v1` strategy definition |

Neither replay path opens a venue connection or dispatches an order.

## Line format rules

- UTF-8, one JSON object per line.
- Blank lines and lines whose first non-whitespace character is `#` are skipped, so
  fixture files can carry a comment header.
- `venue` is `SCREAMING_SNAKE_CASE`: `"HYPERLIQUID"`, `"BINANCE_PM"`, `"IBKR"`.
- `rust_decimal::Decimal` fields are JSON strings (`"30.00"`), not numbers, so venue
  precision survives the round trip. Integer and boolean fields are JSON scalars.
- Invalid JSON aborts the replay and names the offending line number.

## `hype.jsonl` — normalized `MarketEvent`

`MarketEvent` is an externally tagged serde enum, so every line names its variant and
carries the flat fields of the matching struct from `pg-marketdata`.

### `Trade`

```json
{"Trade":{"venue":"HYPERLIQUID","asset":"HYPE","ts_event_ns":1030000000000,"ts_recv_ns":1030000000000,"price":"30.00","quantity":"5","aggressor":"Buy","sequence":1}}
```

| Field | Type | Notes |
| --- | --- | --- |
| `venue` | enum | venue label |
| `asset` | string | venue-scoped asset name |
| `ts_event_ns` | u64 | venue event time, nanoseconds |
| `ts_recv_ns` | u64 | local receive time, nanoseconds |
| `price` | decimal string | |
| `quantity` | decimal string | |
| `aggressor` | enum | `Buy`, `Sell` or `Unknown` |
| `sequence` | u64 or null | `null` when the venue exposes no trustworthy monotonic sequence |

### `BestBidAsk`

```json
{"BestBidAsk":{"venue":"HYPERLIQUID","asset":"HYPE","ts_event_ns":1030000000000,"ts_recv_ns":1030000000000,"bid_price":"30.00","bid_quantity":"20","ask_price":"30.01","ask_quantity":"15","sequence":null}}
```

Fields are `venue`, `asset`, `ts_event_ns`, `ts_recv_ns`, `bid_price`,
`bid_quantity`, `ask_price`, `ask_quantity`, `sequence`.

### `L2Book`

```json
{"L2Book":{"venue":"HYPERLIQUID","asset":"HYPE","ts_event_ns":1000000000000,"ts_recv_ns":1000000000000,"bids":[{"price":"30.00","quantity":"2","order_count":1}],"asks":[{"price":"30.01","quantity":"3","order_count":1}],"sequence":null,"is_snapshot":true}}
```

`bids`/`asks` are arrays of `BookLevel` = `price` (decimal string), `quantity`
(decimal string), `order_count` (u64 or null). `is_snapshot` marks a full book
snapshot as opposed to an incremental update.

### `Candle`

```json
{"Candle":{"venue":"HYPERLIQUID","asset":"HYPE","interval_ns":300000000000,"start_ns":1000000000000,"end_ns":1300000000000,"ts_recv_ns":1300000000000,"open":"29.60","high":"30.20","low":"29.50","close":"30.00","volume":"1000","trades":120}}
```

Fields are `venue`, `asset`, `interval_ns`, `start_ns`, `end_ns`, `ts_recv_ns`,
`open`, `high`, `low`, `close`, `volume` (decimal strings) and `trades` (u64).
A candle carries no `ts_event_ns`; `end_ns` is its event time.

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl
make strategy-replay EVENTS=data/replay/hype.jsonl STRATEGY_DIR=../data/replay/strategies
```

Each line is routed to every strategy instance whose instrument matches it. The
compatibility automation path prints records with `path = "legacy_automation"` and the
portable policy path prints records with `path = "portable_policy_live"`; the summary
line on stderr counts events, signals, non-noop decisions and policy frames.

Replay does not simulate fills. The policy path is evaluated against a flat
`PositionView` here, so exit rules demonstrate evaluation rather than position feedback;
the live daemon builds that view from the simulated venue instead.

## `policy_features.jsonl` — `PolicyReplayFrame`

```json
{"instrument":{"venue":"HYPERLIQUID","asset":"HYPE"},"features":{"values":{"spread_bps":5,"momentum_bps":25,"volume_ratio":1.4}},"position":{"net_quantity":"0","average_entry_price":null,"filled_entries":0,"unrealized_return":null,"peak_return":null},"now_ns":2}
```

| Field | Required | Type | Notes |
| --- | --- | --- | --- |
| `instrument` | yes | `AssetKey` | `venue` + `asset`; selects the policy instances |
| `features` | no | `FeatureFrame` | `values` maps normalized feature name → JSON number |
| `position` | no | `PositionView` | defaults to flat |
| `now_ns` | yes | u64 | evaluation timestamp in nanoseconds |

`PositionView` fields:

| Field | Type | Notes |
| --- | --- | --- |
| `net_quantity` | decimal string | signed; negative is short |
| `average_entry_price` | decimal string or null | |
| `filled_entries` | u32 | number of fills that built the position |
| `unrealized_return` | number or null | fractional return, not basis points |
| `peak_return` | number or null | running peak of the same series |

Position features are addressable both bare (`unrealized_return`) and namespaced
(`position.unrealized_return`). A predicate whose feature is absent from the frame is
reported in the rule's `missing_features` and the rule does not match, so partial
feature state can never create exposure.

```bash
make policy-replay FEATURES=data/replay/policy_features.jsonl
make policy-replay FEATURES=data/replay/policy_features.jsonl STRATEGY_DIR=../data/replay/strategies
```

Output records use `path = "portable_policy_fixture"` and carry the `entry` and
`exit` decisions produced by the same `PolicyEngine` the live path uses.

## `strategies/hype_policy.toml`

Hyperliquid/HYPE only, so it replays without the multi-venue example (which the shadow
daemon refuses because only Hyperliquid and IBKR have runtime market-data sources). It
declares:

- filter `liquidity`: `spread_bps <= 20`;
- long entry `momentum_volume_long`: `momentum_bps >= 20` and `volume_ratio >= 1.25`;
- exit `risk_or_momentum_exit` (`any`): `position.unrealized_return <= -0.10` or
  `momentum_bps <= -50`.

The definition also carries an `[automation]` section, so the same file exercises both
the legacy score path and the portable rule graph during `make strategy-replay`.
Because it declares entries and exits, the definition is policy-driven and the legacy
path's `Submit` decisions are suppressed in the live daemon.

## Commands and paths

`STRATEGY_DIR` defaults to `../strategies`. Both Makefile targets run from `rust/`,
so file arguments are relative to the repository root.

```bash
# Equivalent direct commands
cd rust
PG_INSTANCE_ID=local-strategy-replay PG_STRATEGY_DIR=../data/replay/strategies \
  cargo run -p pg-core -- --replay-market-events ../data/replay/hype.jsonl
PG_INSTANCE_ID=local-policy-replay PG_STRATEGY_DIR=../data/replay/strategies \
  cargo run -p pg-core -- --replay-policy-features ../data/replay/policy_features.jsonl
```

On a host without a local Rust toolchain, run the same cargo commands in the pinned
toolchain image — see the repository README.
