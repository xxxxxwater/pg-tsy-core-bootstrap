"""Deterministic Jev -> Laya failover tests. No external network or trading venue."""

import json

import pytest

from pg_tsy.ml.jev_client import JevObservation, JevUnavailable
from pg_tsy.ml.system_one_failover import (
    JevLayaFailoverClient,
    LayaClient,
    LayaUnavailable,
)

NOW = 1_800_000_000_000_000_000
STATE = {"instrument": "BINANCE_PM:BTCUSDC", "spread_bucket": "tight"}
LAYA_BUILD = "ec8409e542941bb4bb649d5fec00d4cec96ae024"


def laya_reply(**changes):
    payload = {
        "model": "laya-rl-agent",
        "answers": {
            "direction": {
                "type": "choice",
                "choice": "neutral",
                "probabilities": {"up": 0.2, "down": 0.2, "neutral": 0.6},
                "confidence": 0.61,
            }
        },
        "usage": {"input_tokens": 11, "output_tokens": 0},
        "routing": {"model": "typed-decisions", "reason": "explicit model"},
    }
    payload.update(changes)
    return json.dumps(payload).encode()


def laya_client(payload=None, *, clock=None, build_id=LAYA_BUILD):
    def transport(body, key, timeout):
        assert key == ""
        assert timeout == 0.2
        request = json.loads(body)
        assert request["model"] == "convaiinnovations/laya-typed-decisions"
        assert request["state"] == STATE
        assert set(request["questions"]["direction"]["criteria"]) == {
            "up",
            "down",
            "neutral",
        }
        return laya_reply() if payload is None else payload

    return LayaClient(
        endpoint="http://127.0.0.1:8000/v1/systemone",
        build_id=build_id,
        transport=transport,
        clock_ns=clock or (lambda: NOW),
    )


class Primary:
    def __init__(self, result=None, error=None):
        self.result = result
        self.error = error
        self.calls = 0

    def evaluate(self, state, *, source_event_ns, ttl_ns):
        self.calls += 1
        assert state == STATE
        assert source_event_ns == NOW - 100
        assert ttl_ns == 1000
        if self.error:
            raise self.error
        return self.result


class Fallback:
    def __init__(self, result=None, error=None):
        self.result = result
        self.error = error
        self.calls = 0

    def evaluate(self, state, *, source_event_ns, ttl_ns):
        self.calls += 1
        assert state == STATE
        assert source_event_ns == NOW - 100
        assert ttl_ns == 1000
        if self.error:
            raise self.error
        return self.result


def observation(model):
    return JevObservation(
        instrument="BINANCE_PM:BTCUSDC",
        model=model,
        source_event_ns=NOW - 100,
        received_ns=NOW,
        expires_ns=NOW + 900,
        choice="neutral",
        probabilities={"up": 0.2, "down": 0.2, "neutral": 0.6},
        provider_confidence=0.6,
        input_tokens=10,
        output_tokens=0,
    )


def test_laya_validates_wire_shape_and_preserves_build_provenance():
    result = laya_client().evaluate(STATE, source_event_ns=NOW - 100, ttl_ns=1000)
    assert result.choice == "neutral"
    assert result.model == f"laya@{LAYA_BUILD}:laya-rl-agent:typed-decisions"
    assert result.usable_at(NOW)
    assert result.output_tokens == 0
    assert not hasattr(result, "order_intent")


@pytest.mark.parametrize(
    "payload",
    [
        b"not json",
        laya_reply(model=""),
        laya_reply(answers={}),
        laya_reply(
            answers={
                "direction": {
                    "type": "choice",
                    "choice": "up",
                    "probabilities": {"up": 0.1, "down": 0.2, "neutral": 0.7},
                    "confidence": 0.8,
                }
            }
        ),
        laya_reply(usage={"input_tokens": -1, "output_tokens": 0}),
        laya_reply(routing={"model": 123}),
    ],
)
def test_invalid_laya_response_fails_closed(payload):
    with pytest.raises(LayaUnavailable):
        laya_client(payload).evaluate(STATE, source_event_ns=NOW - 100, ttl_ns=1000)


def test_remote_plain_http_is_rejected_before_any_request():
    with pytest.raises(ValueError, match="HTTPS"):
        LayaClient(
            endpoint="http://example.com/v1/systemone",
            build_id=LAYA_BUILD,
            transport=lambda *_: b"",
        )


def test_unpinned_laya_is_explicit_in_observation_provenance():
    result = laya_client(build_id=None).evaluate(
        STATE, source_event_ns=NOW - 100, ttl_ns=1000
    )
    assert result.model.startswith("laya@unpinned:")


def test_primary_success_never_calls_fallback():
    primary = Primary(result=observation("jev-1.13.0"))
    fallback = Fallback(result=observation("laya@pinned:model"))
    result = JevLayaFailoverClient(primary, fallback).evaluate(
        STATE, source_event_ns=NOW - 100, ttl_ns=1000
    )
    assert result.provider == "jev"
    assert not result.used_fallback
    assert primary.calls == 1 and fallback.calls == 0


def test_jev_unavailable_uses_laya_exactly_once():
    primary = Primary(error=JevUnavailable("timeout"))
    fallback = Fallback(result=observation("laya@pinned:model"))
    result = JevLayaFailoverClient(primary, fallback).evaluate(
        STATE, source_event_ns=NOW - 100, ttl_ns=1000
    )
    assert result.provider == "laya"
    assert result.primary_failed and result.used_fallback
    assert primary.calls == 1 and fallback.calls == 1


def test_both_providers_unavailable_never_synthesize_neutral():
    primary = Primary(error=JevUnavailable("timeout"))
    fallback = Fallback(error=LayaUnavailable("down"))
    with pytest.raises(JevUnavailable, match="all System-One"):
        JevLayaFailoverClient(primary, fallback).evaluate(
            STATE, source_event_ns=NOW - 100, ttl_ns=1000
        )
    assert primary.calls == 1 and fallback.calls == 1
