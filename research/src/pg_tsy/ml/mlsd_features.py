"""Offline MLSD feature engineering for a *single* venue and asset.

Research-only boundary: no trading clients, network calls, exchange credentials,
signal publication or changes to pg-core/OMS. MLSD is an optional dependency.
"""
from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass
from pathlib import Path

import numpy as np
import pandas as pd
import polars as pl

SCHEMA_VERSION = "pg-tsy.mlsd.features.v1"
UPSTREAM_VERSION = "0.2.1"


@dataclass(frozen=True, slots=True)
class FeatureConfig:
    window: int = 8
    n_splits: int = 3
    gap: int = 0
    price_column: str = "mid"
    volume_column: str = "trade_qty"
    label_column: str = "label"
    label_end_column: str = "label_end_ns"

    def validate(self) -> None:
        if self.window < 4 or self.n_splits < 2 or self.gap < 0:
            raise ValueError("window >= 4, n_splits >= 2, gap >= 0 required")
        names = (
            self.price_column, self.volume_column,
            self.label_column, self.label_end_column,
        )
        if not all(isinstance(name, str) and name.strip() for name in names):
            raise ValueError("feature and label column names must be nonempty")
        if len(set(names)) != len(names):
            raise ValueError("price, volume, label and label_end must be distinct")


@dataclass(frozen=True, slots=True)
class FeatureFold:
    number: int
    feature_columns: tuple[str, ...]
    train_features: pl.DataFrame
    train_targets: pl.DataFrame
    validation_features: pl.DataFrame
    validation_targets: pl.DataFrame


@dataclass(slots=True)
class FeatureBatch:
    structured: object
    labels: pd.Series
    label_ends: pd.Series
    venue: str
    asset: str
    source_sha256: str
    config: FeatureConfig

    def split(self):
        """Yield strictly past-only, event-purged folds with train-fitted features."""
        try:
            from MLSD import __version__ as version
            from MLSD.model_selection import PurgedEventTimeSeriesSplit
        except ImportError as exc:
            raise RuntimeError("Install the optional MLSD extra: pip install -e '.[mlsd]'") from exc
        if version != UPSTREAM_VERSION:
            raise RuntimeError(f"MLSD {UPSTREAM_VERSION} required; installed {version}")
        cv = PurgedEventTimeSeriesSplit(
            self.label_ends, n_splits=self.config.n_splits, gap=self.config.gap
        )
        for number, (train_ids, validation_ids) in enumerate(cv.split(self.structured)):
            if np.intersect1d(train_ids, validation_ids).size:
                raise ValueError("train/validation index overlap")
            train = self.structured.take(train_ids)
            validation = self.structured.take(validation_ids)
            train_y = self.labels.iloc[train_ids]
            train.fit(y=train_y)
            x_train = train.transform()
            x_validation = train.transform(validation)
            columns = tuple(str(name) for name in x_train.columns)
            if not columns or columns != tuple(str(n) for n in x_validation.columns):
                raise ValueError("feature schema differs between training and validation")
            if len(set(columns)) != len(columns):
                raise ValueError("MLSD emitted duplicate feature names")
            yield FeatureFold(
                number=number,
                feature_columns=columns,
                train_features=_feature_table(x_train, train.index),
                train_targets=_target_table(self, train_ids),
                validation_features=_feature_table(x_validation, validation.index),
                validation_targets=_target_table(self, validation_ids),
            )


def _feature_table(data: pd.DataFrame, index: pd.DatetimeIndex) -> pl.DataFrame:
    dense = data.apply(
        lambda series: (
            series.sparse.to_dense()
            if isinstance(series.dtype, pd.SparseDtype) else series
        )
    )
    array = dense.to_numpy(dtype=np.float64)
    if array.ndim != 2 or not np.isfinite(array).all():
        raise ValueError("nonfinite or malformed MLSD feature output; refusing export")
    return pl.DataFrame({
        "ts_event_ns": index.asi8.tolist(),
        **{str(column): array[:, offset].tolist()
           for offset, column in enumerate(data.columns)},
    })


def _target_table(batch: FeatureBatch, ids: np.ndarray) -> pl.DataFrame:
    return pl.DataFrame({
        "ts_event_ns": batch.structured.index.take(ids).asi8.tolist(),
        "label": batch.labels.iloc[ids].astype(float).tolist(),
        "label_end_ns": batch.label_ends.iloc[ids].array.asi8.tolist(),
    })


def build_feature_batch(
    market: pl.DataFrame, config: FeatureConfig | None = None,
) -> FeatureBatch:
    """Create backward-looking price/volume windows without reading future labels.

    Each row's market timestamp is when that row becomes observable.
    Labels are stored separately and are never passed to an MLSD transformer.
    Data must be sorted, single-instrument, UTC epoch nanoseconds.
    """
    config = config or FeatureConfig()
    config.validate()
    if not isinstance(market, pl.DataFrame):
        raise TypeError("market must be a Polars DataFrame")
    needed = {
        "ts_event_ns", "venue", "asset", config.price_column,
        config.volume_column, config.label_column, config.label_end_column,
    }
    missing = needed - set(market.columns)
    if missing:
        raise ValueError(f"missing required market columns: {sorted(missing)}")
    if market.height < config.window + config.n_splits * 2:
        raise ValueError("not enough market rows for windows and CV folds")
    if any(market[name].null_count() for name in needed):
        raise ValueError("market input contains null values")
    venue, asset = market["venue"].unique().to_list(), market["asset"].unique().to_list()
    if len(venue) != 1 or len(asset) != 1 or not venue[0] or not asset[0]:
        raise ValueError("MLSD batches must contain exactly one venue and asset")
    timestamps = market["ts_event_ns"].to_numpy()
    ends = market[config.label_end_column].to_numpy()
    if timestamps.dtype.kind not in "iu" or ends.dtype.kind not in "iu":
        raise TypeError("event and label-end timestamps must be integer nanoseconds")
    if (np.any(timestamps <= 0) or np.any(np.diff(timestamps) <= 0)):
        raise ValueError("event timestamps must be strictly increasing and positive")
    if np.any(ends <= timestamps):
        raise ValueError("label end must be strictly after its own event start")
    try:
        price = np.asarray(market[config.price_column].to_numpy(), dtype=np.float64)
        quantity = np.asarray(market[config.volume_column].to_numpy(), dtype=np.float64)
        labels = np.asarray(market[config.label_column].to_numpy(), dtype=np.float64)
    except (TypeError, ValueError) as exc:
        raise ValueError("price, quantity, and labels must be numeric") from exc
    if (
        not np.isfinite(price).all() or not np.isfinite(quantity).all()
        or not np.isfinite(labels).all() or np.any(price <= 0)
        or np.any(quantity < 0)
    ):
        raise ValueError("prices/quantity/labels must be finite and physical")

    try:
        from MLSD import SData, SDataFrame, __version__
    except ImportError as exc:
        raise RuntimeError("Install the optional MLSD extra: pip install -e '.[mlsd]'") from exc
    if __version__ != UPSTREAM_VERSION:
        raise RuntimeError(f"MLSD {UPSTREAM_VERSION} required; installed {__version__}")

    offset = config.window - 1
    index = pd.DatetimeIndex(pd.to_datetime(timestamps[offset:], unit="ns", utc=True))
    label_ends = pd.Series(
        pd.to_datetime(ends[offset:], unit="ns", utc=True), index=index,
    )
    # Price windows express relative log returns, avoiding absolute price scale.
    # The final price/volume sample is at the event timestamp, never afterward.
    returns = [
        np.diff(np.log(price[i - offset:i + 1])).tolist()
        for i in range(offset, len(price))
    ]
    volumes = [
        quantity[i - offset:i + 1].tolist()
        for i in range(offset, len(quantity))
    ]
    structured = SDataFrame([
        SData(returns, index=index, column="price_log_returns", dtype="Series"),
        SData(volumes, index=index, column="volume_distribution", dtype="Bag"),
    ])
    # Stable dataset lineage includes raw values, chosen columns and MLSD ABI.
    fields = sorted(needed)
    canonical = json.dumps({
        "schema": SCHEMA_VERSION, "mlsd_version": UPSTREAM_VERSION,
        "config": asdict(config),
        "rows": market.select(fields).to_dicts(),
    }, sort_keys=True, separators=(",", ":"), allow_nan=False)
    fingerprint = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
    return FeatureBatch(
        structured=structured,
        labels=pd.Series(labels[offset:], index=index, name="label"),
        label_ends=label_ends, venue=venue[0], asset=asset[0],
        source_sha256=fingerprint, config=config,
    )


def write_feature_folds(batch: FeatureBatch, output_dir: str | Path) -> Path:
    """Export isolated features/targets, provenance and per-file SHA256 hashes.

    No output file is written unless every CV fold has passed validation.
    Existing output directories are rejected to avoid accidental overwrites.
    """
    folds = list(batch.split())
    output = Path(output_dir)
    if output.exists():
        raise FileExistsError(f"MLSD output already exists: {output}")
    output.mkdir(parents=True, exist_ok=False)
    records = []
    for fold in folds:
        entries = {}
        for name in (
            "train_features", "train_targets",
            "validation_features", "validation_targets",
        ):
            filename = f"fold-{fold.number:02d}/{name}.parquet"
            destination = output / filename
            destination.parent.mkdir(parents=True, exist_ok=True)
            frame = getattr(fold, name)
            frame.write_parquet(destination)
            entries[name] = {
                "path": filename,
                "rows": frame.height,
                "sha256": hashlib.sha256(destination.read_bytes()).hexdigest(),
            }
        records.append({
            "fold": fold.number,
            "feature_columns": list(fold.feature_columns),
            "artifacts": entries,
        })
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "mode": "research-only",
        "source_sha256": batch.source_sha256,
        "mlsd_version": UPSTREAM_VERSION,
        "venue": batch.venue,
        "asset": batch.asset,
        "config": asdict(batch.config),
        "folds": records,
    }
    manifest_path = output / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return manifest_path
