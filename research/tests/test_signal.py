from unittest.mock import patch

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


def zero_epoch_signal() -> Signal:
    return Signal(
        alpha_id="a.v1",
        asset="SOLUSDT",
        venue="BINANCE_PM",
        score=0.2,
        confidence=0.8,
        horizon_ms=1,
        created_at_ns=0,
        expires_at_ns=1_000_000,
        signal_id="zero-epoch",
    )


def test_explicit_clock_including_zero_never_reads_wall_time() -> None:
    signal = zero_epoch_signal()
    with patch("pg_tsy.signal.models.time.time_ns", side_effect=AssertionError("wall clock read")):
        assert not signal.is_expired(0)
        assert not signal.is_expired(999_999)
        assert signal.is_expired(1_000_000)
        assert signal.is_expired(1_000_001)


def test_omitted_and_none_clock_use_wall_time() -> None:
    signal = zero_epoch_signal()
    for now_ns, expired in [(0, False), (999_999, False), (1_000_000, True)]:
        with patch("pg_tsy.signal.models.time.time_ns", return_value=now_ns) as clock:
            assert signal.is_expired() is expired
            clock.assert_called_once_with()
            clock.reset_mock()
            assert signal.is_expired(None) is expired
            clock.assert_called_once_with()
