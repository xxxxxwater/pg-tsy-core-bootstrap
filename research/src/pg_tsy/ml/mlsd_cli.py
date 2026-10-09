"""Credential-free CLI for isolated MLSD research containers."""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import polars as pl

from .mlsd_features import FeatureConfig, build_feature_batch, write_feature_folds


def _smoke() -> int:
    count = 84
    step = 3_600_000_000_000
    ts = 1_770_000_000_000_000_000 + np.arange(count, dtype=np.int64) * step
    frame = pl.DataFrame({
        "ts_event_ns": ts,
        "venue": ["BINANCE_PM"] * count,
        "asset": ["BTCUSDT"] * count,
        "mid": 100.0 + np.arange(count) * 0.05 + np.sin(np.arange(count) / 4),
        "trade_qty": (np.arange(count) % 13 + 1).astype(float),
        "label": (np.arange(count) % 2).astype(float),
        "label_end_ns": ts + 3 * step,
    })
    batch = build_feature_batch(frame)
    folds = list(batch.split())
    if len(folds) != 3 or not all(len(f.feature_columns) == 28 for f in folds):
        raise ValueError("MLSD smoke did not produce expected folds and feature schema")
    print(json.dumps({
        "status": "ok", "mode": "research-only", "mlsd_version": "0.2.1",
        "folds": len(folds), "feature_count": len(folds[0].feature_columns),
        "source_sha256": batch.source_sha256,
    }, sort_keys=True))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        prog="pg-tsy-mlsd", description="Offline market feature engineering (no trading)"
    )
    parser.add_argument("--smoke", action="store_true", help="synthetic no-I/O smoke test")
    parser.add_argument("--input", type=Path, help="single-venue, single-asset market parquet")
    parser.add_argument("--output", type=Path, help="new output folder for fold artifacts")
    parser.add_argument("--window", type=int, default=8)
    parser.add_argument("--splits", type=int, default=3)
    parser.add_argument("--gap", type=int, default=0)
    args = parser.parse_args()
    if args.smoke:
        if args.input or args.output:
            parser.error("--smoke must not use --input or --output")
        return _smoke()
    if not args.input or not args.output:
        parser.error("both --input and --output are required without --smoke")
    if not args.input.is_file():
        parser.error(f"input market parquet not found: {args.input}")
    config = FeatureConfig(window=args.window, n_splits=args.splits, gap=args.gap)
    batch = build_feature_batch(pl.read_parquet(args.input), config=config)
    manifest = write_feature_folds(batch, args.output)
    print(json.dumps({
        "status": "ok", "mode": "research-only",
        "manifest": str(manifest), "source_sha256": batch.source_sha256,
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
