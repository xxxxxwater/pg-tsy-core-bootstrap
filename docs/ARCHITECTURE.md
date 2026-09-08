# Architecture

## Objective

Build a TSY-like *three-path* quant stack that remains understandable by one human maintainer:

- **Factor path**: discover, evaluate and version statistical factors.
- **ML path**: train/validate short-horizon order-book and order-flow models.
- **HFT/execution path**: deterministic Rust state machine for market data, risk, OMS, execution and recovery.

## Hard boundary

The only thing research exports to live trading is a **versioned artifact or signal**. Research never owns live orders or positions.

```text
Research Plane                            Live Plane
--------------                            ----------
Parquet/S3                                venue websocket/rest
   |                                            |
Polars                                      MarketData
   |                                            |
Factor ------+                            State / Features
             |                                  |
ML ----------+----> signal.v1 ----------> RiskEngine
                                                |
                                               OMS
                                                |
                                           Execution
                                                |
                                            Exchange
                                                |
                                        Reconcile/Journal
```

## Why a monorepo

For a one-person team, a monorepo makes contract changes, replay fixtures, CI and agent context easier to reason about. Separate repositories are only justified when access control or independent release cadence becomes a real requirement.

## Why not microservices first

Network boundaries create failure modes. V1 prefers:

- in-process Rust channels inside the live core;
- PostgreSQL for durable metadata/checkpoints;
- S3/Parquet for bulk data;
- explicit process boundary between research and live trading.

Introduce gRPC/SHM only for a measured latency/throughput requirement.

## Position ownership

A venue position is not enough to infer ownership. Ownership is reconstructed from strategy identity, client order identifiers, journaled intents/fills and venue history. Anything unresolved is `Unknown`, which blocks **new strategy exposure** but must not blindly block safe/manual position management.

## Failure model

The live core distinguishes:

- known success;
- known rejection;
- unknown external outcome;
- stale/sequence-gapped market data;
- database/journal unavailable;
- account reconciliation mismatch.

Unknown is not success. Recovery must query venue truth before resuming exposure-increasing actions.
