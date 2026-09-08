# Roadmap

## Phase 0 — Foundation (this scaffold)

- Versioned signal contract.
- Python research package boundaries.
- Rust domain crates for risk/OMS/execution/reconcile/journal/replay.
- CI and local PostgreSQL.
- Live trading disabled by default.

## Phase 1 — Data + factor research

- Binance/Hype historical collectors → Parquet partitioning.
- Canonical trade/L2 schemas.
- Polars factor engine.
- IC, turnover, correlation, regime and transaction-cost evaluation.
- Factor registry and experiment manifests.

Exit: reproducible factor run from immutable dataset snapshot.

## Phase 2 — Rust shadow core

- Production market-data sequencing/recovery.
- Persistent journal + PostgreSQL checkpoints.
- Full OMS state machine including partial fill/unknown/cancel-replace.
- Account/position ownership model.
- Binance PM read-only adapter.
- Shadow order decisions compared against the existing live system.

Exit: deterministic parity/reconciliation report over real market sessions.

## Phase 3 — Binance PM canary

- Official Rust SDK adapter.
- PM account/risk semantics.
- Client-order ownership tags.
- restart/kill-9/network/database failure injection.
- reduce-only emergency path.
- tiny allowlisted canary.

Exit: unattended acceptance criteria explicitly documented and met.

## Phase 4 — LOB / ML

- L2/L3 datasets and labels.
- DeepLOB baseline before more complex models.
- PyTorch model registry and walk-forward validation.
- Cost-aware scoring: edge - fee - spread - slippage - adverse selection.
- optional ONNX/Rust inference for latency-critical models.

## Phase 5 — Hyperliquid + execution research

- Official Rust SDK adapter.
- Local order-book feeds where justified.
- hftbacktest-style queue/latency replay.
- JAX-LOB/AlphaTrade experiments for execution policies.

## Explicit non-goals until needed

- Kubernetes/EKS.
- Kafka cluster.
- dozens of services.
- active-active multi-region trading.
- RL deciding portfolio direction before strong supervised/statistical baselines exist.
