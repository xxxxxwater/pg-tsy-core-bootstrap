from pg_tsy.signal.models import Signal


def test_signal_expiry() -> None:
    signal = Signal(
        alpha_id="a.v1",
        asset="SOLUSDT",
        venue="BINANCE_PM",
        score=0.2,
        confidence=0.8,
        horizon_ms=100,
        created_at_ns=1_000,
        expires_at_ns=101_000_000,
        signal_id="s1",
    )
    assert not signal.is_expired(100_000_000)
    assert signal.is_expired(101_000_000)


def test_invalid_confidence_rejected() -> None:
    try:
        Signal(
            alpha_id="a.v1", asset="SOLUSDT", venue="BINANCE_PM", score=0.2,
            confidence=2.0, horizon_ms=100, created_at_ns=1, expires_at_ns=2,
            signal_id="s2",
        )
    except ValueError:
        return
    raise AssertionError("invalid confidence was accepted")
