"""Deterministic tests; never call TypeSafe or a trading venue."""

import json

import pytest

from pg_tsy.ml.jev_client import JevClient, JevUnavailable

NOW = 1_800_000_000_000_000_000
STATE = {"instrument": "BINANCE_PM:BTCUSDC", "spread_bucket": "tight"}


def reply(**changes):
    response = {
        "model": "jev-1.13.0",
        "answers": {
            "direction": {
                "type": "choice",
                "choice": "neutral",
                "probabilities": {"up": 0.2, "down": 0.2, "neutral": 0.6},
                "confidence": 0.55,
            }
        },
        "usage": {"input_tokens": 10, "output_tokens": 2},
    }
    response.update(changes)
    return json.dumps(response).encode()


def client(payload=None, *, clock=None):
    def transport(body, key, timeout):
        assert key == "test-key"
        assert timeout == 0.35
        request = json.loads(body)
        assert request["model"] == "jev-1.13.0"
        assert request["questions"]["direction"]["type"] == "choice"
        assert set(request["questions"]["direction"]["criteria"]) == {
            "up", "down", "neutral"
        }
        return reply() if payload is None else payload

    return JevClient("test-key", transport=transport, clock_ns=clock or (lambda: NOW))


def test_valid_advisory_never_becomes_order():
    observation = client().evaluate(STATE, source_event_ns=NOW - 100, ttl_ns=1000)
    assert observation.choice == "neutral"
    assert observation.model == "jev-1.13.0"
    assert observation.usable_at(NOW)
    assert not observation.usable_at(NOW + 900)
    assert observation.input_tokens == 10
    assert not hasattr(observation, "order_intent")


@pytest.mark.parametrize(
    "payload",
    [
        b"not json",
        b"[1,2,3]",
        reply(model="jev-2.0"),
        reply(answers={}),
        reply(answers={"direction": {"type": "noul", "noul": 1}}),
        reply(answers={"direction": {
            "type": "choice", "choice": "up", "confidence": 0.9,
            "probabilities": {"up": 0.1, "down": 0.3, "neutral": 0.6},
        }}),
        reply(answers={"direction": {
            "type": "choice", "choice": "neutral", "confidence": 0.9,
            "probabilities": {"up": 0.1, "down": 0.1, "neutral": 0.1},
        }}),
        reply(answers={"direction": {
            "type": "choice", "choice": "neutral", "confidence": float("nan"),
            "probabilities": {"up": 0.2, "down": 0.2, "neutral": 0.6},
        }}),
        reply(usage={"input_tokens": -1, "output_tokens": 0}),
    ],
)
def test_invalid_provider_data_fails_closed(payload):
    with pytest.raises(JevUnavailable):
        client(payload).evaluate(STATE, source_event_ns=NOW - 100, ttl_ns=1000)


def test_stale_or_future_snapshot_never_calls_provider():
    for timestamp in (NOW - 1000, NOW + 1, 0):
        with pytest.raises(JevUnavailable):
            client().evaluate(STATE, source_event_ns=timestamp, ttl_ns=1000)


def test_wrong_symbol_rejected():
    with pytest.raises(JevUnavailable):
        client().evaluate({"instrument": "BINANCE_PM:BTCUSDT"}, source_event_ns=NOW, ttl_ns=1000)


def test_delayed_response_rejected():
    times = iter([NOW, NOW + 2000])
    with pytest.raises(JevUnavailable, match="expired"):
        client(clock=lambda: next(times)).evaluate(STATE, source_event_ns=NOW, ttl_ns=1000)


def test_transport_exception_is_not_an_order_or_a_fake_neutral():
    def broken(*_args):
        raise JevUnavailable("rate limited")

    model = JevClient("test-key", transport=broken, clock_ns=lambda: NOW)
    with pytest.raises(JevUnavailable, match="rate limited"):
        model.evaluate(STATE, source_event_ns=NOW, ttl_ns=1000)
