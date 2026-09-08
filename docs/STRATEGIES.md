# Strategy definitions and runtime

`pg-tsy-core` separates **strategy policy** from **live execution machinery**.

A strategy may change its universe, features, filters, entry/exit rules and sizing without changing Risk, OMS, venue execution, reconciliation or recovery code.

The standard boundary is:

```text
Venue adapters
    ↓
AssetKey + MarketEvent + PositionView
    ↓
FeatureFrame
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

Each instance owns its own factor state, signal sequence, strategy state, position ownership and active order intent.

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
feature = "unrealized_return"
op = "lte"
value = -0.10
```

Missing features never cause an entry rule to match. Entry filters gate **new exposure only**; exit rules are evaluated independently so an entry screen cannot disable management of an already-owned position.

See `strategies/portable_multi_venue.toml` for a complete multi-venue definition.

## Existing online automation path

`AutomatedStrategy` remains the current online score path:

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

This path remains supported while the portable policy graph is wired into the production runtime orchestration. Do not bypass Risk/OMS/reconcile just to make the new graph live sooner.

## Reusable factors

The portable policy module now includes venue-neutral helpers for common formulas including EMA, EWO-style EMA spread, close momentum, RSI, VWAP and rolling volume ratio. More indicators should be added as individually named formulas with fixture tests rather than hidden inside one strategy class.

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

## Validate definitions

From the repository root:

```bash
make strategy-validate
```

This parses every `strategies/*.toml`, validates parameters, expands the universe and compiles the portable policy definition. It does not place orders.

Equivalent direct command:

```bash
cd rust
PG_INSTANCE_ID=local-strategy-validate \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core
```

## Run a strategy locally against market events

The existing replay path executes the online Rust automation pipeline:

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl
```

The input is newline-delimited normalized `MarketEvent` JSON. Replay does **not** route orders to a venue.

The portable policy graph is being integrated into the same replay/runtime path; the standard remains that local policy evaluation and real venue routing share normalized inputs but real execution always passes durable Risk/OMS/recovery gates.

## Editing and reload safety

`StrategyRegistry::reload_file` validates the replacement definition before swap.

Direct reload is rejected if an old instance has:

- non-zero owned position;
- active order intent;
- entering/exiting state;
- `SafeHold` or `Unknown` unresolved state.

Flat or halted instances can be replaced. Production hot reload should evolve toward `validate → drain → reconcile → atomic swap` without transferring ambiguous ownership to the new definition.

## Live runtime boundary

Portable strategy infrastructure is now present, but full unattended live orchestration is still a P0 hardening milestone. Every live decision must continue through journal-before-dispatch, Risk, OMS, execution, continuous reconcile and recovery.

Never add a convenience runner that bypasses those layers just to make a strategy "live" faster.
