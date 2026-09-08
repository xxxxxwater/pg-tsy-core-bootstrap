from __future__ import annotations

import polars as pl

from .base import FactorDefinition


def compute_factor(frame: pl.DataFrame, factor: FactorDefinition) -> pl.DataFrame:
    """Append one versioned factor column to a frame.

    The expression itself always emits `factor_value`; this function renames it to
    the immutable qualified factor id so multiple versions can coexist safely.
    """
    return frame.with_columns(factor.expression()).rename(
        {"factor_value": factor.qualified_id}
    )
