# Strategy automation

The production path is intentionally small and deterministic:

```text
MarketDataSource
  -> Trades / BBO / L2 / Candle
  -> RollingFactorEngine
  -> Signal(signal.v1)
  -> StrategyMachine
  -> OrderIntent
  -> Risk / OMS / Execution
```

## Base subscriptions

Every automated strategy can declare its required feeds from configuration. The baseline live microstructure strategy requests:

- trades / tick-by-tick prints;
- best bid/ask;
- L2 market depth;
- candles (5 seconds by default so the same contract can be used with IBKR realtime bars).

Venue adapters normalize these into `pg_marketdata::MarketEvent`; factor and strategy code never consumes venue SDK types directly.

## Online factors

`pg-strategy::factors::RollingFactorEngine` computes bounded, allocation-light rolling state suitable for the live Rust runtime:

- rolling VWAP and VWAP deviation in basis points;
- signed trade-volume imbalance;
- BBO spread in basis points;
- top-N L2 quantity imbalance;
- candle momentum;
- realized volatility.

Directional factors are normalized and combined using frozen weights. Spread, volatility, warm-up progress and factor availability affect confidence. Low-confidence snapshots never create a tradable signal.

These online factors are deliberately simple. More expensive factor discovery, model training and hyperparameter optimization remain in the local Python research plane.

## Research/live boundary

Python research may discover or tune parameters, but deployment exports only reviewed immutable values such as windows, scales, weights and strategy thresholds. The AWS live image must not train PyTorch/JAX models or run Optuna.

A production parameter change should therefore follow:

```text
local research
  -> walk-forward / robustness checks
  -> frozen parameter artifact
  -> review
  -> shadow runtime
  -> paper/canary
  -> live
```

## Signal throttling and TTL

`AutomatedStrategy` enforces:

- minimum factor confidence;
- minimum signal emission interval;
- signal TTL;
- signal horizon metadata;
- unique live signal IDs.

This prevents every market-data packet from becoming an order decision while preserving tick-level factor state.

## Fill correctness

The strategy state machine tracks cumulative partial fills. Exposure is updated from actual filled quantity, not requested quantity. A terminal fill with an unexpected quantity enters `SAFE_HOLD`; overfills enter `Unknown` and require reconciliation.

No automated strategy may infer or reduce manually owned positions. Ownership remains a reconciliation/risk concern outside factor calculation.
