# Market-data pipeline

The live/research schema is intentionally event-first. Candles are a derived view, not the source of truth.

```text
venue websocket / TWS stream
        |
        +--> trades --------------------+
        |                               |
        +--> best bid/ask / L2 deltas --+--> normalized tick stream
                                        |
                                        +--> candle aggregation
                                        +--> microstructure factors
                                        +--> Parquet research dataset
                                        +--> live strategy state
```

## Granularity

The system can start with websocket K-lines for coarse strategies, but the canonical path moves downward to individual events:

1. venue K-line/candle stream;
2. trade-by-trade ticks;
3. best-bid/ask quotes;
4. L2 book deltas/snapshots;
5. optional L3/L4 order events where the venue exposes them reliably.

Never reconstruct tick-level truth from candles when tick data exists.

## Canonical timestamps

- `ts_event_ns`: venue/event timestamp normalized to Unix nanoseconds.
- `ts_recv_ns`: local receive timestamp where available.
- sequence numbers are preserved for gap detection.

## Initial factors

The Python research library includes small transparent baselines before ML:

- VWAP deviation;
- top-of-book imbalance;
- spread in basis points;
- microprice deviation;
- log return;
- rolling realized volatility;
- signed trade imbalance.

These are intentionally simple. Every factor must be versioned and evaluated out-of-sample before it can feed a production signal.
