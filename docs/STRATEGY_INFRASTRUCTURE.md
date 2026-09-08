# Strategy infrastructure

`pg-tsy-core` treats strategies as portable policy definitions that run on normalized market/account state. Venue SDKs remain behind adapters.

## Design rule

```text
Binance / Hyperliquid / IBKR
        ↓ adapter normalization
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

Strategy code must not import Binance, Hyperliquid or IBKR SDK types. A rule should be reusable across venues when the required normalized feature exists.

## Universe formats

The original single-venue format remains supported:

```toml
[universe]
venue = "HYPERLIQUID"
assets = ["HYPE", "SOL"]
```

The standard multi-venue form is:

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

Each instrument expands into an independent strategy instance with its own factor windows, state machine, position ownership and active order intent. The strategy template stays the same; venue-specific contract/symbol details stay in adapters or instrument metadata/configuration.

## Policy kernel

`pg-strategy::policy` provides the reusable strategy boundary:

- `FeatureFrame`: named normalized feature values;
- `Predicate`: threshold comparisons that fail closed when input is missing;
- `StrategyContext`: `AssetKey + FeatureFrame + PositionView + time`;
- `EntryRule` / `ExitRule` / `StrategyFilter` traits;
- fixed and DCA sizing policies;
- venue-neutral factor helpers such as EMA, EWO, momentum, RSI, VWAP and rolling volume ratio.

The policy kernel intentionally does not place orders. It produces strategy decisions that still pass Risk, OMS, durable dispatch, execution and reconciliation.

## Legacy/Freqtrade strategy migration

A Freqtrade strategy such as `VWAP_V4_GRID.py` is treated as one strategy instance and migration fixture, not as framework architecture.

It is acceptable to change its structure to fit this framework:

- move indicator formulas into reusable factor modules;
- move BTC/pair regime checks into filters;
- move tagged entry expressions into named entry rules;
- move DCA callbacks into sizing policies;
- move custom exits and stoploss functions into exit/risk policies;
- move exchange/orderbook access into normalized liquidity inputs;
- move mutable parameters into TOML strategy definitions.

The migration should preserve the strategy's approximate decision logic and threshold semantics unless a change is explicitly documented and retested. Do not preserve Freqtrade lifecycle coupling just for source-code similarity.

A typical migration is:

```text
VWAP_V4_GRID.py
├─ indicators             → policy/factors + online feature state
├─ informative BTC/pair   → normalized regime/filter inputs
├─ populate_entry_trend   → named EntryRule implementations
├─ adjust_trade_position  → DcaLadderSizing / position policy
├─ custom_exit            → named ExitRule implementations
├─ custom_stoploss        → risk/stop policy
└─ confirm_trade_entry    → normalized liquidity/pre-trade risk gate
```

The same migrated logic can then be instantiated for Binance PM crypto, Hyperliquid crypto or IBKR instruments without importing any venue SDK into the strategy crate.

## Important portability caveat

Portable infrastructure does **not** mean identical production parameters across assets or venues. Tick size, lot size, trading hours, liquidity, fees, leverage and market microstructure differ. Strategy definitions may override parameters per instrument while reusing the same policy code.

## Current boundary

The policy kernel and multi-venue universe are infrastructure primitives. Existing `AutomatedStrategy` remains the current online signal path. The next integration step is to compile declarative policy definitions into the runtime strategy graph and route normalized feature updates into them before `Signal/PositionTarget` generation.
