# VWAP_V4 migration instance

`VWAP_V4_GRID.py` is a **strategy migration example**, not a core framework dependency.

The source strategy may be structurally modified to fit `pg-tsy-core` as long as its approximate trading logic and threshold semantics are preserved or any intentional changes are explicitly documented and retested.

## What should remain logically recognizable

The original strategy combines:

- base-timeframe technical factors;
- BTC/base-market regime context;
- pair-specific 1h trend context;
- several tagged long-entry families;
- liquidity/slippage entry protection;
- a reserved-capital DCA ladder;
- trend/structure exits;
- profit/trailing/deadfish exits;
- a piecewise protective stop.

Those concepts should survive migration even when the implementation shape changes.

## What may change for framework compatibility

The following Freqtrade-specific structure does **not** need to survive:

- `populate_indicators` as one large callback;
- `populate_entry_trend` as one large boolean dataframe expression;
- direct `dp.get_pair_dataframe` calls inside the strategy;
- direct `dp.orderbook` calls inside the strategy;
- `Trade` persistence as the source of truth for ownership;
- `adjust_trade_position` as the DCA execution mechanism;
- exchange-specific order callbacks inside strategy code.

They should map to portable infrastructure instead:

```text
Freqtrade callback / state              pg-tsy-core target
---------------------------              ------------------
populate_indicators                  -> feature providers / FactorFrame
BTC 5m informative                   -> regime feature source
pair 1h informative                  -> timeframe feature source
populate_entry_trend                 -> named EntryRule graph
confirm_trade_entry orderbook access -> liquidity filter / pre-trade Risk
custom_stake_amount                  -> SizingPolicy
adjust_trade_position                -> DCA / PositionTarget policy
populate_exit_trend                  -> ExitRule graph
custom_exit                          -> ExitRule graph
custom_stoploss                      -> stop/risk policy
Trade / filled orders                -> OMS + ownership + reconcile state
```

## Venue portability

The migrated strategy must consume only normalized inputs such as:

```text
AssetKey
MarketEvent
FeatureFrame
PositionView
```

It must not import Binance, Hyperliquid or IBKR SDK types.

That allows the same migrated policy implementation to be instantiated for, for example:

```text
BINANCE_PM:ETHUSDT
HYPERLIQUID:HYPE
IBKR:AAPL
```

Venue-specific symbol resolution, contract metadata, tick/lot rules, order routing and account behavior remain adapter/runtime concerns.

## Parity method

Do not claim parity because the Rust source looks similar to the Python source. Prove it with fixtures:

1. export normalized candle/informative/position snapshots from the legacy strategy;
2. compute the equivalent Rust `FeatureFrame`;
3. compare every named entry/exit/DCA decision independently;
4. record intentional differences;
5. only then compare end-to-end replay results.

A useful fixture should include both matches and near-misses around thresholds. This is especially important for EMA/RSI/EWO/VWAP-style calculations where library initialization details can create small numerical differences.

## Current status

The repository now has the portable policy kernel, reusable factor helpers, multi-venue universe expansion and declarative rule graph. The full VWAP_V4 rule set has **not** been declared production-parity yet. It should be ported incrementally as one instance on top of the standard infrastructure.
