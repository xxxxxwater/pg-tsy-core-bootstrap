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
- `[strategy] allow_short` defaults to false (long-only); a short entry only opens exposure when a definition opts in.

### Market data

- Shared Trade/BBO/L2/Candle event model.
- Feed freshness and sequence-gap primitives.
- Hyperliquid official Rust SDK websocket mapping for trades, BBO, L2 and candles.
- IBKR TWS/IB Gateway mapping for tick-by-tick trades, tick-by-tick BBO, market depth and 5-second realtime bars.
- IBKR is a runtime market-data source behind the opt-in `ibkr-marketdata` cargo feature; the shipped image enables it through the `PG_CORE_FEATURES` build arg.
- Venue-aware startup validation: a derived subscription no build can serve fails startup instead of reconnecting forever. Binance PM has no runtime feed.
- Candle resolution is a venue capability table (`supported_candle_intervals`, `candle_interval_supported`, `default_candle_interval`, `describe_candle_interval`). An explicitly configured `candle_interval_ns` a venue cannot serve fails at strategy load; when omitted, each instrument uses its own venue default (Hyperliquid 1m, IBKR 5s) instead of a global 5s.
- Explicit rule that feeds without trustworthy venue sequence data keep `sequence=None` rather than fabricating gap evidence.
- Subscription supervisor/runtime primitives for required feed ownership, reconnect and health transitions.

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
- `ShadowExecutionAdapter` (`rust/crates/pg-execution/src/shadow.rs`) implements the same `ExecutionAdapter` contract against an in-process book: idempotent adoption by client order id, `ShadowFillMode::{Rest, ImmediateFill}`, simulated positions with average entry, and a software reduce-only guard.

### Durable execution path (shadow)

- `pg-core --serve` accepts `PG_RUN_MODE=shadow` only; `paper` and `live` are refused at startup, as is any configuration with real-venue routing enabled.
- The daemon builds an `AdapterRegistry` of shadow adapters (Hyperliquid, IBKR, Binance PM) and a `pg_orchestrator::DurableExecution`.
- Every decision goes `pg_risk::evaluate_order` → `DurableExecution::dispatch` → simulated venue. The order record and intent event are written **before** the adapter call, and fencing is asserted again immediately before it.
- `PG_SHADOW_FILL_MODE` selects `rest` (default, acknowledge only) or `immediate` (fill on acknowledgement). Neither sends a real order. The old "shadow order intent (not dispatched)" message is gone.
- One decision engine owns dispatch per definition: a definition whose compiled rule graph is defined dispatches through the policy path, and the legacy score machine's `Submit` is suppressed (`PolicyEngine::is_defined`, `PolicyInstance::is_policy_driven`).
- The simulated venue is marked from each normalized market event and a real `pg_strategy::policy::PositionView` is built from it (net quantity, average entry, filled entries, unrealized return, peak return).
- Policy exits are always reduce-only; entries only open from flat; a short entry is suppressed unless the definition enables `allow_short`.
- A rule graph fails closed when a referenced feature is missing (`evaluate_predicates`).

### Runtime control / observability

- All 11 `StartupGate` values are driven by real checks (`apply_execution_gates` plus the heartbeat tick) instead of staying `Pending`.
- `/healthz`, `/readyz` and `/metrics` (Prometheus text format) on the health/control listener; the snapshot exposes `open_orders`, `orders_journaled_total` and `blocking_gates`.
- `POST /admin/reload` on the same listener re-validates every strategy file before swapping anything. It is an operator surface that can never submit, cancel or flatten.
- `PG_SHUTDOWN_POLICY` (`preserve` / `cancel_resting` / `flatten_owned`) is implemented in `apply_shutdown_policy` and applied when the daemon exits, cancelling resting orders before any reduce-only flatten.
- `make health`, `make ready`, `make metrics` and `make reload` wrap those endpoints.

### Recovery / ownership / storage

- Strategy/manual/unknown position ownership model.
- Venue+asset-scoped reconciliation and SAFE_HOLD primitives.
- Detection primitives for unknown ownership, local/venue order mismatch and filled-quantity mismatch.
- PostgreSQL runtime lease and fencing token.
- Lease heartbeat.
- Append-only journal/checkpoint structures.
- Durable order records, ownership state, reconciliation history and command audit schema/store primitives.
- Cold-start recovery design based on checkpoint + journal + venue truth.

### Build / CI

- Toolchain pinned to Rust **1.98.1** in both the Dockerfile and every CI job.
- `rust/Cargo.lock` is committed so image builds resolve identical dependency versions; the `rust-lockfile` CI job runs `cargo metadata --locked`.
- `docker-compose.yml` `pg-core` supplies only `["--serve"]` as its command, leaving the binary path to the image ENTRYPOINT.
- Opt-in `ib-gateway` compose service behind the `ibkr` profile, image `ghcr.io/gnzsnz/ib-gateway:stable`, paper API port 4002 / live 4001, read-only API by default.

Current CI exercises:

- Python Ruff + pytest;
- Rust `cargo fmt --check`;
- workspace Clippy with warnings denied;
- workspace tests;
- lockfile drift (`cargo metadata --locked`);
- Hyperliquid SDK feature tests;
- IBKR SDK feature tests;
- Hyperliquid + IBKR feature Clippy;
- Telegram feature compile.

## Not yet production-complete

The following remain blocking for an unattended live/canary declaration:

- continuous reconciliation loop wired into the daemon, applying venue fills back into the OMS. `DurableExecution::reconcile_once` exists and is unit-tested, but the daemon never calls it;
- the durable `OrderRecord` is **not yet updated from venue fills**: the shadow venue fills and holds a simulated position, but the persisted record still shows the submit-time state — under `PG_SHADOW_FILL_MODE=immediate` the venue reports the order filled while the durable record remains `Open` with `filled_quantity` zero until a reconcile pass writes it back;
- end-to-end durable **journal-before-dispatch** enforcement and crash-window proof around every exposure-changing submit (an accepted-but-unpersisted ACK that cannot produce duplicate exposure);
- Telegram `/emergency_exit` wired to authenticated, idempotent flatten + HALT through Risk/OMS/Execution;
- alerting on top of the metrics endpoint (no Prometheus/Grafana alert rules or dashboards are shipped);
- hardened Docker/systemd EC2 deployment and shutdown/restart behavior;
- Binance Portfolio Margin execution/recovery parity with the Hyperliquid/IBKR adapters;
- systematic kill-9, network partition, venue timeout and PostgreSQL failure injection;
- shadow/parity sessions and small-capital allowlisted canary acceptance.

## Current completion boundary

The **durable, journal-before-dispatch shadow execution slice is implemented and CI-tested**. Strategy decisions now pass risk, are journaled before the adapter call, reach a simulated venue and feed a real position view back into the policy graph — with no path to a real exchange in the shipping daemon.

It does **not** by itself prove system-wide exactly-once execution. Two links are still missing:

```text
venue fills
     |
     v
continuous reconcile  <- not called by the daemon yet
     |
     v
durable OrderRecord reflects venue truth
```

and a crash-window proof that an ACK lost between journal and record update cannot produce duplicate exposure.

## Next milestone

**End-to-end reconcile/recovery proof**, followed by emergency control and deployment hardening:

1. continuous reconcile loop writing venue fills back into the OMS;
2. restart/kill-9 crash-window failure-injection tests;
3. Telegram emergency flatten;
4. Binance PM execution/recovery parity;
5. hardened Docker/systemd EC2 deployment and alerting;
6. shadow → paper → tiny canary acceptance.
