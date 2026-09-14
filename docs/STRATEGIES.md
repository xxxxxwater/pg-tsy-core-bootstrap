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

`evaluate_predicates` fails closed: if any predicate of a rule references a feature the
frame does not carry, the rule cannot match and the missing names are reported in
`missing_features`. Partial feature state is never enough to create exposure.

Position state is resolved through `StrategyContext` rather than copied into a venue-specific strategy object. Both `unrealized_return` and `position.unrealized_return` address the normalized position view. In the live daemon that view is built from the simulated venue: net quantity, average entry price, filled entries, unrealized return and running peak return.

See `strategies/portable_multi_venue.toml` for a complete multi-venue definition.

## One decision engine per definition

A definition may carry both an `[automation]` section and a `[[policy.*]]` rule graph.
Only one of them may dispatch:

- `PolicyEngine::is_defined()` is true when the definition declares at least one entry
  or exit rule; `PolicyInstance::is_policy_driven()` mirrors that;
- a policy-driven definition dispatches through the rule graph, and the legacy score
  machine's `Submit` decisions for that definition are suppressed in the daemon;
- a definition with only `[automation]` keeps the legacy score path.

Without this rule two independent decision engines could open the same exposure on one
instrument.

## Long-only by default

`[strategy] allow_short` defaults to `false`:

```toml
[strategy]
id = "momentum-volume-vwap"
order_quantity = "1"
entry_score = 0.35
exit_score = 0.05
# allow_short = true   # opt in; absent means long-only
```

The flag reaches both engines: the legacy `StrategyMachine` only opens a short from
flat when `allow_short` is set, and the policy path suppresses a matched `side = "Sell"`
entry when the definition is long-only. That suppression is a runtime decision with a
debug log, not a load-time error: the rule still exists in the graph, it just cannot open
exposure. Shorting is an exposure-increasing action, so it must be asked for explicitly
rather than inherited from a symmetric score threshold.

## Candle resolution is a venue capability

`pg-marketdata` owns the table of candle resolutions a venue can actually serve
(`supported_candle_intervals`, `candle_interval_supported`, `default_candle_interval`,
`describe_candle_interval`, `describe_candle_intervals`). It is a capability table, not a
preference:

- an explicitly configured `[automation] candle_interval_ns` that the instrument's venue
  cannot serve fails at strategy load, naming the supported resolutions;
- when the interval is omitted, each instrument uses its own venue default — the venue's
  smallest supported resolution — so one template can span venues with different candle
  contracts (Hyperliquid 1m, IBKR 5s) instead of a global 5s;
- a permanently rejected subscription would otherwise leave the daemon reconnecting
  forever and never ready, with no obvious cause.

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

The portable policy path runs alongside it and owns its own graph-derived feature
subscriptions. In the shadow daemon it dispatches — through exactly the same gates as the
legacy path:

```text
policy decision (entry from flat / reduce-only exit)
   ↓
pg_risk::evaluate_order
   ↓
DurableExecution::dispatch (journal before adapter)
   ↓
shadow venue
```

Neither engine can bypass Risk, the journal or the OMS, and only one of them dispatches
for a given definition. Exits are always `ReduceOnly`; entries only open from flat; the
policy path tracks a working client order id per strategy instance so the same entry is
not re-emitted on every tick.

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
make policy-replay FEATURES=data/replay/policy_features.jsonl STRATEGY_DIR=../data/replay/strategies
```

or:

```bash
cd rust
PG_INSTANCE_ID=local-policy-replay \
PG_STRATEGY_DIR=../strategies \
cargo run -p pg-core -- --replay-policy-features ../data/replay/policy_features.jsonl
```

This evaluates the portable rule graph without touching any venue. The frame format is
documented in [`data/replay/README.md`](../data/replay/README.md).

## Replay live-style market events

Market-event replay now executes both the compatibility automation path and the portable policy feature runtime:

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl
make strategy-replay EVENTS=data/replay/hype.jsonl STRATEGY_DIR=../data/replay/strategies
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

The running daemon exposes this over its health/control listener:

```bash
make reload   # POST http://127.0.0.1:8080/admin/reload
```

The HTTP endpoint only enqueues `ControlCommand::ReloadStrategies`; it cannot submit,
cancel or flatten. The daemon re-reads every `*.toml` in the strategy directory, validates
each replacement and only then swaps, so a rejected reload leaves the running set
untouched and records the reason in the health snapshot's `last_error`. Reload does not
drain: an instance with owned position, active intent or non-flat state is still refused.

## Live runtime boundary

Portable feature planning, live normalized policy evaluation and real market
subscriptions now run inside the shadow daemon: the derived `FeedSpec` inventory drives
the market-data tasks, one decision engine per definition produces intents, and every
intent continues through Risk → journal-before-dispatch → OMS → execution.

What is still missing for unattended live orchestration: a continuous reconcile loop that
writes venue fills back into the durable order records, and the crash-window proof around
an ACK lost between journal and record update. `pg-core --serve` therefore still refuses
`paper` and `live`, and the only execution adapter it registers is the in-process shadow
venue.

Never add a convenience runner that bypasses Risk, the journal, the OMS or reconcile just
to make a strategy "live" faster.
