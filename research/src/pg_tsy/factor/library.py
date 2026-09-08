import polars as pl

from .base import FactorDefinition, FactorRegistry


def _vwap_deviation() -> pl.Expr:
    return ((pl.col("mid") - pl.col("vwap")) / pl.col("vwap")).alias("factor_value")


def _order_book_imbalance() -> pl.Expr:
    denom = pl.col("bid_qty") + pl.col("ask_qty")
    return ((pl.col("bid_qty") - pl.col("ask_qty")) / denom).alias("factor_value")


def _spread_bps() -> pl.Expr:
    mid = (pl.col("bid_px") + pl.col("ask_px")) / 2
    return ((pl.col("ask_px") - pl.col("bid_px")) / mid * 10_000).alias("factor_value")


def _microprice_deviation() -> pl.Expr:
    denom = pl.col("bid_qty") + pl.col("ask_qty")
    microprice = (
        pl.col("ask_px") * pl.col("bid_qty") + pl.col("bid_px") * pl.col("ask_qty")
    ) / denom
    mid = (pl.col("bid_px") + pl.col("ask_px")) / 2
    return ((microprice - mid) / mid).alias("factor_value")


def _log_return() -> pl.Expr:
    return pl.col("mid").log().diff().alias("factor_value")


def _realized_volatility_60() -> pl.Expr:
    returns = pl.col("mid").log().diff()
    return returns.rolling_std(window_size=60, min_samples=20).alias("factor_value")


def _signed_trade_imbalance_50() -> pl.Expr:
    signed_qty = pl.col("trade_qty") * pl.col("trade_sign")
    numerator = signed_qty.rolling_sum(window_size=50, min_samples=10)
    denominator = pl.col("trade_qty").rolling_sum(window_size=50, min_samples=10)
    return (numerator / denominator).alias("factor_value")


def default_registry() -> FactorRegistry:
    registry = FactorRegistry()
    definitions = [
        FactorDefinition(
            factor_id="factor.vwap_deviation",
            version=1,
            description="Relative deviation of mid from VWAP",
            expression=_vwap_deviation,
        ),
        FactorDefinition(
            factor_id="factor.order_book_imbalance",
            version=1,
            description="Top-level bid/ask quantity imbalance",
            expression=_order_book_imbalance,
        ),
        FactorDefinition(
            factor_id="factor.spread_bps",
            version=1,
            description="Best bid/ask spread in basis points",
            expression=_spread_bps,
        ),
        FactorDefinition(
            factor_id="factor.microprice_deviation",
            version=1,
            description="Microprice deviation from top-of-book mid",
            expression=_microprice_deviation,
        ),
        FactorDefinition(
            factor_id="factor.log_return",
            version=1,
            description="One-event log return of mid price",
            expression=_log_return,
        ),
        FactorDefinition(
            factor_id="factor.realized_volatility_60",
            version=1,
            description="Rolling 60-event standard deviation of log returns",
            expression=_realized_volatility_60,
        ),
        FactorDefinition(
            factor_id="factor.signed_trade_imbalance_50",
            version=1,
            description="Rolling signed-volume imbalance over 50 trades",
            expression=_signed_trade_imbalance_50,
        ),
    ]
    for definition in definitions:
        registry.register(definition)
    return registry
