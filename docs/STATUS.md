# Project status

Current stage: **Phase 0 / scaffold**.

Implemented:

- Python package boundaries for data, factors, ML and signals.
- `signal.v1` JSON contract.
- Rust workspace/domain boundaries.
- Minimal signal TTL and risk-gate logic.
- Minimal OMS state machine.
- Market-data sequence gap primitive.
- Ownership-aware reconciliation primitive.
- JSONL development journal/replay primitives.
- Binance PM and Hyperliquid adapter boundaries.
- Local PostgreSQL, AWS Terraform skeleton and CI.

Not yet production-complete:

- No real exchange order submission.
- No production PostgreSQL journal/checkpoint implementation.
- No Binance PM websocket/reconcile implementation.
- No Hyperliquid websocket/reconcile implementation.
- No full L2/L3 storage schema or collector.
- No ML training pipeline/model registry.
- No deterministic shadow-parity harness against an existing bot.

The next milestone is **read-only venue integration + deterministic shadow core**, not live trading.
