#!/usr/bin/env python3
from __future__ import annotations

import argparse
from pathlib import Path

import polars as pl

from pg_tsy.ml.local_runtime import detect_accelerator
from pg_tsy.ml.trainer import train_mlp_classifier


def main() -> int:
    parser = argparse.ArgumentParser(description="Local-only baseline ML trainer")
    parser.add_argument("--parquet", type=Path, required=True)
    parser.add_argument("--features", nargs="+", required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--artifact", type=Path, default=Path("artifacts/model.pt"))
    parser.add_argument("--epochs", type=int, default=50)
    args = parser.parse_args()

    frame = pl.read_parquet(args.parquet).select([*args.features, args.label]).drop_nulls()
    print(f"accelerator={detect_accelerator().name} rows={frame.height}")
    result = train_mlp_classifier(
        frame.select(args.features).to_numpy(),
        frame[args.label].to_numpy(),
        args.artifact,
        epochs=args.epochs,
    )
    print(result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
