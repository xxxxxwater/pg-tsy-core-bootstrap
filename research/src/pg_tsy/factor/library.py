import polars as pl

from .base import FactorDefinition, FactorRegistry


def _vwap_deviation() -> pl.Expr:
    return ((pl.col("mid") - pl.col("vwap")) / pl.col("vwap")).alias("factor_value")


def _order_book_imbalance() -> pl.Expr:
    denom = pl.col("bid_qty") + pl.col("ask_qty")
    return ((pl.col("bid_qty") - pl.col("ask_qty")) / denom).alias("factor_value")


def default_registry() -> FactorRegistry:
    registry = FactorRegistry()
    registry.register(
        FactorDefinition(
            factor_id="factor.vwap_deviation",
            version=1,
            description="Relative deviation of mid from VWAP",
            expression=_vwap_deviation,
        )
    )
    registry.register(
        FactorDefinition(
            factor_id="factor.order_book_imbalance",
            version=1,
            description="Top-level bid/ask quantity imbalance",
            expression=_order_book_imbalance,
        )
    )
    return registry
