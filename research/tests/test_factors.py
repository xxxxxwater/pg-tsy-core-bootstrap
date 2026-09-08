from pg_tsy.factor.library import default_registry


def test_default_factors_are_versioned() -> None:
    assert default_registry().list() == [
        "factor.order_book_imbalance.v1",
        "factor.vwap_deviation.v1",
    ]
