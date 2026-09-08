# Project status

Current stage: **P0 production slice / execution-and-recovery hardening**.

The repository is no longer a scaffold, but it is also **not yet an unattended-production release**.

## Implemented

### Research / strategy automation

- Python package boundaries for data, factors, ML, tuning and signals.
- Versioned `signal.v1` contract.
- Local-only PyTorch/JAX/Optuna-oriented training path; AWS live runtime does not train or tune.
- Rust online factor engine for VWAP/VWAP deviation, trade imbalance, spread, L2 imbalance, momentum and realized volatility.
- Automated subscription → factor → signal → strategy state-machine path.
- Signal TTL, warmup, confidence, spread/volatility gates and throttling.
- Fill-aware strategy-owned position tracking.

### Market data

- Shared Trade/BBO/L2/Candle event model.
- Feed freshness and sequence-gap primitives.
- Hyperliquid official Rust SDK websocket mapping for trades, BBO, L2 and candles.
- IBKR TWS/IB Gateway mapping for tick-by-tick trades, tick-by-tick BBO, market depth and 5-second realtime bars.
- Explicit rule that feeds without trustworthy venue sequence data keep `sequence=None` rather than fabricating gap evidence.

### OMS / execution

- OMS lifecycle with open/partial-fill/filled/cancel/unknown semantics.
- Stable deterministic client order identity from persisted intent.
- Shared `ExecutionAdapter` with submit/cancel/open-orders/positions/read-side recovery.
- Hyperliquid execution adapter:
  - persisted intent UUID → venue `cloid`;
  - pre-submit lookup;
  - ambiguous-submit lookup/recovery;
  - no blind replacement POST when outcome remains unknown;
  - partial-fill/open/historical/account-state mapping.
- IBKR execution adapter using community `ibapi 4.0.1`:
  - stable `order_ref`;
  - recovery through open orders → completed orders → executions;
  - ambiguous submit/cancel becomes fail-closed `Unknown`;
  - optional software reduce-only guard with cross-through-flat protection;
  - `SellLong` correctly normalized as a sell-side action.

### Recovery / ownership / storage

- Strategy/manual/unknown position ownership model.
- Venue+asset-scoped reconciliation and SAFE_HOLD primitives.
- Detection primitives for unknown ownership, local/venue order mismatch and filled-quantity mismatch.
- PostgreSQL runtime lease and fencing token.
- Lease heartbeat.
- Append-only journal/checkpoint structures.
- Durable order records, ownership state, reconciliation history and command audit schema/store primitives.
- Cold-start recovery design based on checkpoint + journal + venue truth.

### CI

Current CI exercises:

- Python Ruff + pytest;
- Rust `cargo fmt --check`;
- workspace Clippy with warnings denied;
- workspace tests;
- Hyperliquid SDK feature tests;
- IBKR SDK feature tests;
- Hyperliquid + IBKR feature Clippy;
- Telegram feature compile.

## Not yet production-complete

The following remain blocking for an unattended live/canary declaration:

- continuous reconciliation loop wired through the complete live runtime;
- end-to-end durable **journal-before-dispatch** enforcement and crash-window proof around every exposure-changing submit;
- full restart recovery orchestration proving that an accepted-but-unpersisted ACK cannot produce duplicate exposure;
- Telegram `/emergency_exit` wired to authenticated, idempotent flatten + HALT through Risk/OMS/Execution;
- production `/healthz` / `/readyz` and Prometheus metrics/alerts;
- hardened Docker/systemd EC2 deployment and shutdown/restart behavior;
- Binance Portfolio Margin execution/recovery parity with the Hyperliquid/IBKR adapters;
- systematic kill-9, network partition, venue timeout and PostgreSQL failure injection;
- shadow/parity sessions and small-capital allowlisted canary acceptance.

## Current completion boundary

The **Hyperliquid `cloid` + IBKR `order_ref` execution/recovery slice is implemented and CI-tested**. This means the adapters have a stable way to recognize a previously submitted order after an ambiguous outcome.

It does **not** by itself prove system-wide exactly-once execution. The next production milestone is to connect those adapter guarantees to the durable runtime protocol:

```text
journal intent / dispatch state
          |
          v
submit with stable venue identity
          |
    ambiguous outcome
          |
          v
restart / continuous reconcile
          |
venue snapshot + history + fills
          |
          v
recover exact owned state
          |
prove no duplicate exposure
```

## Next milestone

**End-to-end reconcile/recovery proof**, followed by emergency control and observability hardening:

1. continuous reconcile loop;
2. journal-before-dispatch orchestration;
3. restart/kill-9 failure-injection tests;
4. Telegram emergency flatten;
5. health/readiness/Prometheus;
6. Docker/systemd EC2 hardening;
7. Binance PM parity;
8. shadow → paper → tiny canary acceptance.
