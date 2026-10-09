"""MLSD offline research integration and no-lookahead invariants."""
import hashlib
import json

import numpy as np
import polars as pl
import pytest

pytest.importorskip("MLSD")
pytest.importorskip("pandas")

from pg_tsy.ml.mlsd_features import FeatureConfig, build_feature_batch, write_feature_folds


def market_rows(n=84):
    step = 3_600_000_000_000
    ts = 1_770_000_000_000_000_000 + np.arange(n, dtype=np.int64) * step
    return pl.DataFrame({
        "ts_event_ns": ts, "venue": ["BINANCE_PM"] * n,
        "asset": ["BTCUSDT"] * n,
        "mid": 100.0 + 0.06 * np.arange(n) + np.sin(np.arange(n) / 3) * 0.7,
        "trade_qty": (np.arange(n) % 9 + 1).astype(float),
        "label": (np.arange(n) % 2).astype(float),
        "label_end_ns": ts + 3 * step,
    })


def test_event_purged_oos_features_never_mix_target_columns():
    batch = build_feature_batch(market_rows(), FeatureConfig(window=8, n_splits=3, gap=2))
    folds = list(batch.split())
    assert len(folds) == 3
    assert len(batch.source_sha256) == 64
    for fold in folds:
        assert len(fold.feature_columns) == 28
        assert all(name.startswith(("price_log_returns__", "volume_distribution__"))
                   for name in fold.feature_columns)
        assert "label" not in fold.train_features.columns
        assert "label_end_ns" not in fold.validation_features.columns
        assert fold.train_features.columns == fold.validation_features.columns
        assert fold.train_targets.columns == fold.validation_targets.columns
        assert fold.train_features.height == fold.train_targets.height
        assert fold.validation_features.height == fold.validation_targets.height
        first_test = fold.validation_features["ts_event_ns"][0]
        assert fold.train_features["ts_event_ns"].max() < first_test
        assert fold.train_targets["label_end_ns"].max() < first_test
        assert np.isfinite(fold.validation_features.drop("ts_event_ns").to_numpy()).all()


def test_older_windows_do_not_change_when_future_prices_change():
    original = market_rows()
    modified = original.with_columns(
        pl.when(pl.arange(0, pl.len()) >= 60)
        .then(pl.col("mid") * 8.0)
        .otherwise(pl.col("mid")).alias("mid")
    )
    before = build_feature_batch(original)
    after = build_feature_batch(modified)
    assert before.source_sha256 != after.source_sha256
    for col in range(2):
        for row in range(60 - before.config.window):
            np.testing.assert_array_equal(
                np.asarray(before.structured.data[col].values[row]),
                np.asarray(after.structured.data[col].values[row]),
            )


def test_overlong_event_horizons_are_purged():
    raw = market_rows()
    long = raw.with_columns(
        pl.when(pl.arange(0, pl.len()) < 40)
        .then(pl.col("label_end_ns") + 40 * 3_600_000_000_000)
        .otherwise(pl.col("label_end_ns")).alias("label_end_ns")
    )
    for fold in build_feature_batch(long).split():
        assert fold.train_targets["label_end_ns"].max() < (
            fold.validation_targets["ts_event_ns"][0]
        )


def test_rejects_unsorted_mixed_assets_and_invalid_targets():
    raw = market_rows()
    with pytest.raises(ValueError, match="strictly increasing"):
        build_feature_batch(raw.reverse())
    with pytest.raises(ValueError, match="one venue and asset"):
        build_feature_batch(raw.with_columns(
            pl.when(pl.arange(0, pl.len()) == 10)
            .then(pl.lit("ETHUSDT")).otherwise(pl.col("asset")).alias("asset")
        ))
    with pytest.raises(ValueError, match="strictly after"):
        build_feature_batch(raw.with_columns(pl.col("ts_event_ns").alias("label_end_ns")))
    with pytest.raises(ValueError, match="finite"):
        build_feature_batch(raw.with_columns(pl.lit(float("nan")).alias("mid")))
    with pytest.raises(ValueError, match="physical"):
        build_feature_batch(raw.with_columns(pl.lit(-1.0).alias("trade_qty")))
    with pytest.raises(ValueError, match="physical"):
        build_feature_batch(raw.with_columns(pl.lit(0.0).alias("mid")))
    with pytest.raises(ValueError, match="not enough"):
        build_feature_batch(raw.head(9))
    with pytest.raises(ValueError, match="window"):
        FeatureConfig(window=3).validate()


def test_manifest_and_target_files_are_separate_and_integrity_checked(tmp_path):
    batch = build_feature_batch(market_rows())
    output = tmp_path / "export"
    manifest_path = write_feature_folds(batch, output)
    manifest = json.loads(manifest_path.read_text())
    assert manifest["schema_version"] == "pg-tsy.mlsd.features.v1"
    assert manifest["mode"] == "research-only"
    assert manifest["mlsd_version"] == "0.2.1"
    assert len(manifest["folds"]) == 3
    for fold in manifest["folds"]:
        assert len(fold["feature_columns"]) == 28
        for artifact in fold["artifacts"].values():
            path = output / artifact["path"]
            assert hashlib.sha256(path.read_bytes()).hexdigest() == artifact["sha256"]
        features = pl.read_parquet(output / fold["artifacts"]["train_features"]["path"])
        targets = pl.read_parquet(output / fold["artifacts"]["train_targets"]["path"])
        assert "label" not in features.columns
        assert targets.columns == ["ts_event_ns", "label", "label_end_ns"]
        assert features["ts_event_ns"].to_list() == targets["ts_event_ns"].to_list()
    with pytest.raises(FileExistsError):
        write_feature_folds(batch, output)


def test_empty_training_fold_fails_closed():
    raw = market_rows().with_columns(
        (pl.col("ts_event_ns") + 100 * 3_600_000_000_000).alias("label_end_ns")
    )
    with pytest.raises(ValueError, match="no training"):
        list(build_feature_batch(raw).split())
