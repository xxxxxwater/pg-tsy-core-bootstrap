import polars as pl

from pg_tsy.factor.compute import compute_factor
from pg_tsy.factor.library import default_registry


def test_default_factors_are_versioned() -> None:
    assert default_registry().list() == [
        "factor.log_return.v1",
        "factor.microprice_deviation.v1",
        "factor.order_book_imbalance.v1",
        "factor.realized_volatility_60.v1",
        "factor.signed_trade_imbalance_50.v1",
        "factor.spread_bps.v1",
        "factor.vwap_deviation.v1",
    ]


def test_spread_factor() -> None:
    frame = pl.DataFrame({"bid_px": [99.0], "ask_px": [101.0]})
    factor = default_registry().get("factor.spread_bps.v1")
    out = compute_factor(frame, factor)
    assert out["factor.spread_bps.v1"][0] == 200.0
