# Strategy definitions and runtime

`pg-tsy-core` separates **strategy policy** from **live execution machinery**.

A strategy may change its universe, features, filters, entry/exit rules and sizing without changing Risk, OMS, venue execution, reconciliation or recovery code.

The standard boundary is:

```text
Venue adapters
    ↓
AssetKey + MarketEvent + PositionView
    ↓
Feature Provider Registry
    ↓
normalized FeatureFrame
    ↓
filters → entries → sizing → exits
    ↓
Signal / PositionTarget
    ↓
Risk → OMS → Execution → Reconcile
```

Strategy code must not import Binance, Hyperliquid or IBKR SDK types.

## `strategy.v1`

Strategy files live under `strategies/*.toml` by default. Set `PG_STRATEGY_DIR` to use another directory.

### Legacy single-venue universe

Existing definitions remain valid:

```toml
[universe]
venue = "HYPERLIQUID"
assets = ["HYPE", "SOL"]
```

### Standard multi-venue universe

New portable definitions may declare instruments independently:

```toml
[universe]
[[universe.instruments]]
venue = "BINANCE_PM"
asset = "ETHUSDT"

[[universe.instruments]]
venue = "HYPERLIQUID"
asset = "HYPE"

[[universe.instruments]]
venue = "IBKR"
asset = "AAPL"
```

The same strategy template is expanded into independent instances. A cross-venue template is named with venue and asset so identities cannot collide:

```text
portable-momentum:BINANCE_PM:ETHUSDT
portable-momentum:HYPERLIQUID:HYPE
portable-momentum:IBKR:AAPL
```

Each instance owns its own factor state, feature-provider state, signal sequence, strategy state, position ownership and active order intent.

## Portable policy graph

`pg-strategy::policy` provides venue-neutral policy primitives:

- `FeatureFrame` for named normalized feature values;
- threshold `Predicate`s with fail-closed missing-data behavior;
- `StrategyContext = AssetKey + FeatureFrame + PositionView + time`;
- `StrategyFilter` for entry gates;
- `EntryRule` and `ExitRule`;
- fixed and DCA sizing policies;
- reusable technical factor helpers;
- `PolicyDefinition` / `PolicyEngine` for declarative rule graphs.

Example:

```toml
[[policy.filters]]
id = "liquidity"
mode = "all"

[[policy.filters.predicates]]
feature = "spread_bps"
op = "lte"
value = 20.0

[[policy.entries]]
id = "momentum_volume_long"
side = "Buy"
mode = "all"

[[policy.entries.predicates]]
feature = "momentum_bps"
op = "gte"
value = 20.0

[[policy.entries.predicates]]
feature = "volume_ratio"
op = "gte"
value = 1.25

[[policy.exits]]
id = "risk_exit"
mode = "any"

[[policy.exits.predicates]]
feature = "position.unrealized_return"
op = "lte"
value = -0.10
```

Missing features never cause an entry rule to match. Entry filters gate **new exposure only**; exit rules are evaluated independently so an entry screen cannot disable management of an already-owned position.

Position state is resolved through `StrategyContext` rather than copied into a venue-specific strategy object. Both `unrealized_return` and `position.unrealized_return` address the normalized position view.

See `strategies/portable_multi_venue.toml` for a complete multi-venue definition.

## Feature Provider Registry

The portable graph now derives its live data requirements from the features it actually references.

```text
PolicyDefinition
      ↓
required_features()
      ↓
FeatureProviderRegistry
      ↓
FeaturePlan
      ↓
minimal FeedSpec set
      ↓
MarketEvent
      ↓
LiveFeatureEngine
      ↓
FeatureFrame
      ↓
PolicyEngine
```

The standard registry currently maps normalized feature names to feed dependencies:

| Feature | Live dependency |
| --- | --- |
| `last_price`, `vwap`, `vwap_deviation_bps`, `trade_imbalance` | Trades |
| `spread_bps` | BestBidAsk |
| `book_imbalance` | L2Book |
| `momentum_bps`, `realized_volatility_bps`, `volume_ratio` | Candle |
| `warmup_ratio`, `score`, `confidence` | Trades + BBO + L2 + Candle |
| position fields | PositionView, no market subscription |

For example, a policy containing only:

```text
momentum_bps
volume_ratio
spread_bps
position.unrealized_return
```

derives only:

```text
Candle
BestBidAsk
```

for each configured instrument. It does not subscribe to trades or L2 merely because those feeds are available.

The registry is strict by design. A strategy definition that references a feature with no registered live provider fails during strategy loading instead of starting successfully and silently producing a permanently missing feature. Add a named provider before using a custom live factor.

This is the extension point for future portable indicators such as CTI, CCI, RMI, CMF, ATR or strategy-specific regime features: implement the normalized provider, declare its feed requirements, register its stable feature name, then use that name from strategy definitions.

## Live normalized FeatureFrame

`LiveFeatureEngine` consumes normalized `MarketEvent`s, not exchange SDK objects. Its rolling state produces the same named `FeatureFrame` consumed by fixture replay.

This gives one policy semantic path:

```text
fixture JSON ----------------------+
                                  |
                                  v
                            FeatureFrame
                                  |
                                  v
                             PolicyEngine
                                  ^
                                  |
Venue SDK -> adapter -> MarketEvent
                       -> LiveFeatureEngine
                       -> FeatureFrame
```

That is the parity boundary: fixture tests and live normalized data must agree on feature names and units before a migrated strategy is considered equivalent.

## Existing online automation path

`AutomatedStrategy` remains the compatibility online score path:

```text
MarketEvent
   ↓
RollingFactorEngine
   ↓
EntryFilter
   ↓
Signal
   ↓
StrategyMachine
   ↓
OrderIntent
```

The portable policy path now runs alongside it in market-event replay and owns its own graph-derived feature subscriptions. It is not yet allowed to bypass the production Risk/OMS/reconcile gates or dispatch orders directly.

## Reusable factors

The portable policy module includes venue-neutral helpers for common formulas including EMA, EWO-style EMA spread, close momentum, RSI, VWAP and rolling volume ratio. More indicators should be added as individually named formulas with fixture tests rather than hidden inside one strategy class.

## Legacy/Freqtrade migration

A Freqtrade strategy such as `VWAP_V4_GRID.py` is an **instance and migration fixture**, not framework architecture.

Its structure may be changed to fit the standard runtime while preserving the approximate trading logic and threshold semantics:

```text
Freqtrade strategy                     pg-tsy-core
------------------                     -----------
informative pairs / whitelist       -> universe + normalized timeframe inputs
populate_indicators                 -> feature providers / factor modules
trend/volume preconditions          -> filters
entry tags                          -> named EntryRule graph
position adjustment / DCA           -> sizing / PositionTarget policy
custom exit                         -> ExitRule graph
custom stoploss                     -> stop/risk policy
confirm_trade_entry orderbook       -> liquidity filter + pre-trade Risk
Trade persistence                   -> OMS + ownership + reconcile
```

Direct exchange/data-provider calls should be removed from the strategy during migration. Binance, Hyperliquid and IBKR differences stay behind adapters.

See `examples/strategy_instances/VWAP_V4_MIGRATION.md` for the migration contract. Full VWAP_V4 parity is intentionally not claimed until rule-level fixtures compare the migrated implementation with the legacy behavior.

## Validate definitions and derived subscriptions

From the repository root:

```bash
make strategy-validate
```

This parses every `strategies/*.toml`, validates parameters, expands the universe, compiles the portable policy definition, verifies that every live feature has a registered provider and computes the subscription inventory. It does not place orders.

Equivalent direct command:

```bash
cd rust
PG_INSTANCE_ID=local-strategy-validate \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core
```

## Replay fixture features

For migration/parity tests, supply newline-delimited normalized feature frames:

```bash
make policy-replay FEATURES=data/replay/policy_features.jsonl
```

or:

```bash
cd rust
PG_INSTANCE_ID=local-policy-replay \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core -- --replay-policy-features ../data/replay/policy_features.jsonl
```

This evaluates the portable rule graph without touching any venue.

## Replay live-style market events

Market-event replay now executes both the compatibility automation path and the portable policy feature runtime:

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl
```

The portable path is:

```text
MarketEvent
   ↓
LiveFeatureEngine
   ↓
FeatureFrame
   ↓
PolicyEngine
   ↓
entry / exit decision
```

Output records use `path = "portable_policy_live"`; fixture policy replay uses `path = "portable_policy_fixture"`. This makes it straightforward to compare rule outcomes while keeping order routing disabled.

The input is newline-delimited normalized `MarketEvent` JSON. Replay does **not** route orders to a venue.

## Editing and reload safety

`StrategyRegistry::reload_file` validates the replacement definition before swap, including feature-provider planning.

Direct reload is rejected if an old instance has:

- non-zero owned position;
- active order intent;
- entering/exiting state;
- `SafeHold` or `Unknown` unresolved state.

Flat or halted instances can be replaced. Production hot reload should evolve toward `validate → drain → reconcile → atomic swap` without transferring ambiguous ownership to the new definition.

## Live runtime boundary

Portable feature planning and live normalized policy evaluation are now implemented, but full unattended live orchestration is still a P0 hardening milestone. Real market subscriptions still need to be driven by the derived `FeedSpec` inventory inside the production daemon, and any actionable policy decision must continue through journal-before-dispatch, Risk, OMS, execution, continuous reconcile and recovery.

Never add a convenience runner that bypasses those layers just to make a strategy "live" faster.
