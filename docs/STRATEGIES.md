# Strategy definitions and runtime

`pg-tsy-core` separates **strategy definition** from **live execution machinery**.

A strategy definition may change its universe, factor weights, momentum/volume entry screen and signal thresholds without changing OMS, execution, reconciliation or recovery code.

## `strategy.v1`

Strategy files live under `strategies/*.toml` by default. Set `PG_STRATEGY_DIR` to use another directory.

Example: `strategies/momentum_volume_vwap.toml`.

```toml
schema_version = "strategy.v1"
enabled = true

[universe]
venue = "HYPERLIQUID"
assets = ["HYPE", "SOL"]

[strategy]
id = "momentum-volume-vwap"
order_quantity = "1"
entry_score = 0.35
exit_score = 0.05

[automation]
candle_interval_ns = 300000000000
min_confidence = 0.45
alpha_id = "alpha.momentum-volume-vwap.v1"

[automation.factors]
momentum_weight = 0.40
vwap_weight = 0.20
trade_imbalance_weight = 0.20
book_imbalance_weight = 0.20

[automation.entry_filter]
enabled = true
min_momentum_bps = 20.0
volume_window = 25
min_volume_samples = 25
min_volume_ratio = 1.25
min_trade_imbalance = 0.0
max_spread_bps = 20.0
max_realized_volatility_bps = 180.0
```

A multi-asset definition is expanded into independent strategy instances. The example above becomes:

```text
momentum-volume-vwap:HYPE
momentum-volume-vwap:SOL
```

Each instance owns its own factor windows, selector state, signal sequence, position state and active intent identity.

## Momentum + volume screen

The entry selector currently supports:

- minimum rolling candle momentum in basis points;
- minimum latest-candle volume relative to prior rolling mean;
- minimum aggressive-trade imbalance;
- minimum L2 book imbalance;
- maximum spread;
- maximum realized volatility.

The selector is deliberately an **entry gate**. When an instance is `Flat`, a failing screen blocks new exposure. Once the strategy owns a position, the screen does not disable exit/position-management decisions. Risk, reconcile and emergency controls remain authoritative.

The current `min_momentum_bps` implementation is close-to-close rolling momentum from the Rust candle window. It is not the same formula as Freqtrade EWO. Add EWO as a separately named factor if an exact port is required; do not silently give two different formulas the same name.

## Relationship to `VWAP_V4_GRID.py`

The Freqtrade strategy used as a migration reference combines several concepts that map naturally into this split:

```text
Freqtrade strategy                     pg-tsy-core
------------------                     -----------
informative pairs / whitelist       -> universe + subscriptions
populate_indicators                 -> RollingFactorEngine / factor modules
trend/volume preconditions          -> EntryFilterConfig
entry tags                          -> named signal/rule definitions (future expansion)
position adjustment / DCA           -> position-target / sizing policy (future port)
custom exit / stoploss              -> exit policy + risk/position state
confirm_trade_entry liquidity guard -> pre-trade risk / liquidity gate
Trade persistence                   -> OMS + journal + ownership + reconcile
```

The current configurable example ports the *architecture* of a momentum/volume gate feeding a VWAP/microstructure score. It does not claim byte-for-byte or signal-for-signal parity with the full Freqtrade strategy. Exact migration of EWO, CTI, RMI/CCI, informative 1h regime filters, all tagged entries, DCA ladder and custom exit/stoploss should be implemented as individually named, testable rules before parity is claimed.

## Validate definitions

From the repository root:

```bash
make strategy-validate
```

This parses every `strategies/*.toml`, validates parameters, expands the universe into strategy instances and prints the resulting strategy/subscription inventory. It does not place orders.

Equivalent direct command:

```bash
cd rust
PG_INSTANCE_ID=local-strategy-validate \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core
```

## Run a strategy locally against market events

Market-event replay executes the real Rust path:

```text
MarketEvent
   -> RollingFactorEngine
   -> MomentumVolumeSelector
   -> Signal
   -> StrategyMachine
   -> OrderIntent / Hold / Noop
```

Run:

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl
```

or:

```bash
cd rust
PG_INSTANCE_ID=local-strategy-replay \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core -- --replay-market-events ../data/replay/hype.jsonl
```

The input is newline-delimited JSON, one serialized `MarketEvent` per line. Output is newline-delimited JSON containing factor snapshots, entry-filter result, signal and non-noop strategy decision. Replay does **not** route orders to a venue.

## Editing and reload safety

`StrategyRegistry::reload_file` validates the replacement definition before swap.

Direct reload is rejected if any instance created from the old file has:

- a non-zero owned position;
- an active order intent;
- an entering/exiting state;
- `SafeHold` or `Unknown` unresolved state.

Flat or halted instances can be replaced. This conservative rule prevents a new definition from silently inheriting a position whose entry logic/sizing belongs to an older version.

A later production hot-reload controller may support a staged `drain -> reconcile -> swap` workflow, but it must preserve the same ownership guarantee.

## Live runtime boundary

Strategy definitions are now loadable, routable and locally runnable. Full unattended live orchestration is still a P0 hardening milestone: subscriptions must be connected to venue `MarketDataSource`s and every `OrderIntent` must still pass durable journal-before-dispatch, Risk, OMS, execution, continuous reconcile and recovery gates.

Never add a convenience runner that bypasses those layers just to make a strategy "live" faster.
