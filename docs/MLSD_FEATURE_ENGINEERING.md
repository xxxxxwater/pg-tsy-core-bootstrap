# MLSD feature-engineering layer (offline Research/Shadow)

> **Status:** Source-only MLSD integration for research. No automatic signal
> promotion, no `pg-core` integration, no new order permission, and no
> three-venue real-money acceptance. The existing Rust `Dockerfile`,
> `docker-compose*.yml`, OMS/risk/reconcile adapters and production services
> are unchanged.

## Architecture and ownership

```text
Polars market lake (immutable parquet, ONE venue and asset)
    │  ts_event_ns (UTC ns), mid, trade_qty, label, label_end_ns
    ↓
pg_tsy.ml.mlsd_features (offline, optional MLSD==0.2.1)
    ├── strict timestamps, physical values, entity, schema checks
    ├── backward-only price log-return windows (Series, 21 statistics)
    └── backward-only volume windows (Bag, 7 statistics)
    ↓
PurgedEventTimeSeriesSplit (actual future-label horizon + row gap)
    │   Fitted MLSD transformers only see the training fold
    ↓
fold-XX/train_features.parquet       fold-XX/train_targets.parquet
fold-XX/validation_features.parquet  fold-XX/validation_targets.parquet
    ↓
manifest.json (source hash, feature names, file SHA-256, MLSD version)
    ↓
local-only model training / Dream-RSI experimental evaluation
    X   NOT connected to exchange adapters, signals, order entry or OMS
```

The MLSD dependency is **explicitly optional**, pinned to upstream
`v0.2.1` at Git commit
`85e356c1fb6703fef2b03ac0070c5e0659e8c671`.
The 2026-10 research integration is version
`pg-tsy.mlsd.features.v1`. Core Rust and live Docker images do not
install this dependency.

## Market input contract

A single asset and single venue are mandatory. The input is strictly
increasing (no duplicates) and already normalized to event-observed UTC
epoch **nanoseconds**.

| Field | Type / invariant |
| --- | --- |
| `ts_event_ns` | strictly increasing positive integer UTC nanoseconds |
| `venue`, `asset` | one explicit nonempty value each per input file |
| `mid` | strictly positive, finite numeric observed price |
| `trade_qty` | nonnegative finite numeric executed quantity |
| `label` | precomputed finite supervised target; **not an input feature** |
| `label_end_ns` | integer UTC nanoseconds when the label is finally known; strictly after `ts_event_ns` |

Column names for numeric inputs and labels are overridable in
`FeatureConfig`. A missing field, null, invalid timestamp, mixed asset,
zero price, negative quantity or nonfinite prediction feature fails
closed. Each window includes only samples **at or before** that row's
`ts_event_ns`. No as-of joins or externally derived news/alternative
signals are included automatically. Research must validate their
publication/ingestion times separately before adding them.

Targets are intentionally in **separate Parquet files**, preventing
accidental model training on future labels. Always select feature columns
from each manifest rather than guessing by position.

## Local usage

```bash
cd research
python -m pip install -e '.[dev,mlsd]'
python -m pytest -q tests/test_mlsd_features.py
pg-tsy-mlsd --smoke

pg-tsy-mlsd \
  --input /absolute/path/to/market.parquet \
  --output /absolute/path/to/feature-export \
  --window 16 --splits 4 --gap 2
```

The output folder must not already exist. Each fold's `train_features`
and `validation_features` share an exact feature schema. Corresponding
`train_targets` and `validation_targets` are separate and aligned
using `ts_event_ns`. All Parquet artifact hashes are recorded in
`manifest.json`. Export is refused when any fold has zero safe
training samples or a transformer yields nonfinite output.

Example of programmatic use:

```python
import polars as pl
from pg_tsy.ml.mlsd_features import (
    FeatureConfig, build_feature_batch, write_feature_folds,
)

batch = build_feature_batch(
    pl.read_parquet("market.parquet"),
    FeatureConfig(window=16, n_splits=4, gap=2),
)
manifest_path = write_feature_folds(batch, "./offline-features-v1")
print(manifest_path)
```

## Docker image (offline only)

```bash
docker pull ghcr.io/xxxxxwater/pg-tsy-mlsd:v0.2.0-rc.1
docker run --rm ghcr.io/xxxxxwater/pg-tsy-mlsd:v0.2.0-rc.1
```

To process a local Parquet file, mount an input directory as read-only
and a **new, writable** output parent with appropriate non-root
permissions (UID 10001):

```bash
docker run --rm \
  -v "$PWD/input:/input:ro" \
  -v "$PWD/output:/output" \
  ghcr.io/xxxxxwater/pg-tsy-mlsd:v0.2.0-rc.1 \
  --input /input/market.parquet \
  --output /output/features-001 --window 16 --splits 4 --gap 2
```

New GHCR packages can initially be private; authentication may be
required. No Docker command here launches `pg-core --serve`, creates
orders, connects to exchanges or changes trading state.

## Causality and limits

A train label is eligible only when
`train_label_end_ns < first_validation_ts_event_ns`. The splitter
also enforces a configurable additional bar gap and never uses
future rows for model fitting. Unlike global `fit_transform`, each
fold fits MLSD transformers on its training partition only.

**This is not a full proof of no leakage.** Labels may themselves have
been constructed with revised data, external features may arrive late,
and asset-universe selection can be noncausal. Validate every producer
and perform fully out-of-sample strategy, fees, slippage, latency and
counterfactual replay before proposing any model for execution.
Performance improvement, Sharpe, HFT alpha and venue safety are **not**
claimed from these synthetic tests.

## Release and rollback

- `v0.2.0-rc.1`: MLSD Research/Shadow integration and
  `ghcr.io/xxxxxwater/pg-tsy-mlsd:v0.2.0-rc.1`.
- `v0.1.0-rc.1`: original research/shadow source prerelease, unchanged.
- Upstream `mlsd-structured-data:v0.2.1`: separate upstream MLSD
  package and container, unchanged.

To roll back research usage, stop running the MLSD research container and
consume the earlier research features; **no live state migration exists**
because the MLSD integration does not touch any live database, OMS,
reconciliation state, or trading permissions.
