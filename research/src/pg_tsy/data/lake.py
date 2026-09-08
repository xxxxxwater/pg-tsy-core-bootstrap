from __future__ import annotations

from pathlib import Path
import polars as pl


REQUIRED_MARKET_COLUMNS = {
    "ts_event_ns",
    "venue",
    "asset",
}


def validate_market_frame(frame: pl.DataFrame) -> None:
    missing = REQUIRED_MARKET_COLUMNS - set(frame.columns)
    if missing:
        raise ValueError(f"missing required market columns: {sorted(missing)}")
    if frame["ts_event_ns"].null_count() > 0:
        raise ValueError("ts_event_ns cannot contain nulls")


def write_parquet(frame: pl.DataFrame, path: str | Path) -> None:
    validate_market_frame(frame)
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    frame.write_parquet(path, compression="zstd", statistics=True)


def scan_parquet(path: str | Path) -> pl.LazyFrame:
    return pl.scan_parquet(str(path))
