"""Jev -> Laya research failover for typed System-One decisions.

This module is advisory-only. A successful fallback never creates an OrderIntent and
never bypasses deterministic policy, risk, OMS, or release gates.

Laya exposes a Jev-compatible POST /v1/systemone endpoint, so the fallback keeps the
same state/question contract while preserving provider provenance. Jev is attempted
once. Only an unavailable/invalid Jev result triggers Laya; there are no retries on a
stale market snapshot.
"""

from __future__ import annotations

import json
import math
import os
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlparse

from .jev_client import JevClient, JevObservation, JevUnavailable

DEFAULT_LAYA_ENDPOINT = "http://127.0.0.1:8000/v1/systemone"
DEFAULT_LAYA_MODEL = "convaiinnovations/laya-typed-decisions"
OPTIONS = frozenset({"up", "down", "neutral"})
Transport = Callable[[bytes, str, float], bytes]


class LayaUnavailable(JevUnavailable):
    """Laya cannot provide a fresh validated advisory observation."""


def _bounded_number(value: Any, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (float, int)):
        raise LayaUnavailable(f"invalid {name}")
    number = float(value)
    if not math.isfinite(number) or not 0.0 <= number <= 1.0:
        raise LayaUnavailable(f"invalid {name}")
    return number


def _validate_endpoint(endpoint: str) -> str:
    parsed = urlparse(endpoint)
    if parsed.username or parsed.password:
        raise ValueError("Laya endpoint must not embed credentials")
    if parsed.path != "/v1/systemone" or parsed.query or parsed.fragment:
        raise ValueError("Laya endpoint must be an exact /v1/systemone URL")
    if parsed.scheme == "https" and parsed.hostname:
        return endpoint
    if parsed.scheme == "http" and parsed.hostname in {"127.0.0.1", "localhost", "::1"}:
        return endpoint
    raise ValueError("remote Laya endpoints require HTTPS; HTTP is loopback-only")


def _http_transport(endpoint: str, body: bytes, api_key: str, timeout_s: float) -> bytes:
    headers = {"Content-Type": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    request = urllib.request.Request(endpoint, data=body, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=timeout_s) as response:
            if response.status != 200:
                raise LayaUnavailable("non-200 Laya inference response")
            return response.read(65537)
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise LayaUnavailable("Laya inference transport unavailable") from exc


def _direction_request(state: Mapping[str, Any], model: str) -> dict[str, Any]:
    return {
        "model": model,
        "state": dict(state),
        "questions": {
            "direction": {
                "type": "choice",
                "instructions": (
                    "Classify only the supplied short-horizon order-flow state; "
                    "do not calculate prices, returns or trade sizes. "
                    "If evidence is weak, select neutral."
                ),
                "criteria": {
                    "up": "Buy-side flow dominance",
                    "down": "Sell-side flow dominance",
                    "neutral": "Mixed, insufficient or balanced evidence",
                },
            }
        },
    }


@dataclass(frozen=True)
class FailoverResult:
    """Provider provenance around the existing immutable advisory observation."""

    observation: JevObservation
    provider: str
    primary_failed: bool

    @property
    def used_fallback(self) -> bool:
        return self.provider == "laya"


class LayaClient:
    """Bounded Laya System-One client for shadow/challenger use.

    `build_id` should identify the deployed Laya image or upstream Git commit.
    If omitted, observations are marked `unpinned`; release gates must not allow
    such evidence to promote a strategy.
    """

    def __init__(
        self,
        api_key: str | None = None,
        *,
        endpoint: str | None = None,
        model: str = DEFAULT_LAYA_MODEL,
        build_id: str | None = None,
        timeout_s: float = 0.20,
        transport: Transport | None = None,
        clock_ns: Callable[[], int] = time.time_ns,
    ) -> None:
        self._endpoint = _validate_endpoint(
            endpoint or os.environ.get("LAYA_SYSTEMONE_ENDPOINT", DEFAULT_LAYA_ENDPOINT)
        )
        self._key = api_key if api_key is not None else os.environ.get("LAYA_API_KEY", "")
        self._model = model.strip()
        self._build_id = (build_id or os.environ.get("LAYA_BUILD_ID", "unpinned")).strip()
        if not self._model or not self._build_id or not 0 < timeout_s <= 10:
            raise ValueError("model/build id and a bounded positive timeout are required")
        self._timeout_s = timeout_s
        self._clock_ns = clock_ns
        self._transport = transport or (
            lambda body, key, timeout: _http_transport(self._endpoint, body, key, timeout)
        )

    def evaluate(
        self,
        state: Mapping[str, Any],
        *,
        source_event_ns: int,
        ttl_ns: int,
    ) -> JevObservation:
        now = self._clock_ns()
        if (
            not isinstance(source_event_ns, int)
            or isinstance(source_event_ns, bool)
            or source_event_ns <= 0
            or source_event_ns > now
            or not isinstance(ttl_ns, int)
            or isinstance(ttl_ns, bool)
            or ttl_ns <= 0
            or now - source_event_ns >= ttl_ns
            or state.get("instrument") != "BINANCE_PM:BTCUSDC"
        ):
            raise LayaUnavailable("invalid or stale BTCUSDC snapshot")
        expires = source_event_ns + ttl_ns
        try:
            body = json.dumps(
                _direction_request(state, self._model),
                allow_nan=False,
                separators=(",", ":"),
            ).encode("utf-8")
        except (TypeError, ValueError, OverflowError) as exc:
            raise LayaUnavailable("invalid Laya inference request") from exc

        try:
            raw = self._transport(body, self._key, self._timeout_s)
            if len(raw) > 65536:
                raise LayaUnavailable("oversized Laya inference response")
            payload = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError, TypeError, OverflowError) as exc:
            raise LayaUnavailable("malformed Laya inference response") from exc

        received = self._clock_ns()
        if received >= expires or received < now:
            raise LayaUnavailable("expired Laya inference response")
        if not isinstance(payload, dict):
            raise LayaUnavailable("invalid Laya response object")
        response_model = payload.get("model")
        if not isinstance(response_model, str) or not response_model.strip():
            raise LayaUnavailable("missing Laya model provenance")

        routing = payload.get("routing")
        route_model = None
        if routing is not None:
            if not isinstance(routing, dict):
                raise LayaUnavailable("invalid Laya routing metadata")
            candidate = routing.get("model")
            if candidate is not None and not isinstance(candidate, str):
                raise LayaUnavailable("invalid Laya routing model")
            route_model = candidate

        answers = payload.get("answers")
        if not isinstance(answers, dict) or set(answers) != {"direction"}:
            raise LayaUnavailable("incorrect Laya answer schema")
        answer = answers["direction"]
        if not isinstance(answer, dict) or answer.get("type") != "choice":
            raise LayaUnavailable("incorrect Laya choice schema")
        probs = answer.get("probabilities")
        if not isinstance(probs, dict) or set(probs) != OPTIONS:
            raise LayaUnavailable("incorrect Laya choice distribution")
        clean = {option: _bounded_number(value, option) for option, value in probs.items()}
        if abs(sum(clean.values()) - 1.0) > 0.001:
            raise LayaUnavailable("non-normalized Laya probabilities")
        choice = answer.get("choice")
        if choice not in OPTIONS or clean[choice] < max(clean.values()) - 0.001:
            raise LayaUnavailable("inconsistent Laya winning choice")
        confidence = _bounded_number(answer.get("confidence"), "Laya provider confidence")

        usage = payload.get("usage")
        if not isinstance(usage, dict):
            raise LayaUnavailable("missing Laya usage")
        tokens = (usage.get("input_tokens"), usage.get("output_tokens"))
        if any(not isinstance(x, int) or isinstance(x, bool) or x < 0 for x in tokens):
            raise LayaUnavailable("invalid Laya token usage")

        route_suffix = f":{route_model}" if route_model else ""
        provenance = f"laya@{self._build_id}:{response_model}{route_suffix}"
        return JevObservation(
            instrument="BINANCE_PM:BTCUSDC",
            model=provenance,
            source_event_ns=source_event_ns,
            received_ns=received,
            expires_ns=expires,
            choice=choice,
            probabilities=clean,
            provider_confidence=confidence,
            input_tokens=tokens[0],
            output_tokens=tokens[1],
        )


class JevLayaFailoverClient:
    """Try Jev once, then Laya once, on the same still-fresh snapshot."""

    def __init__(self, primary: JevClient, fallback: LayaClient) -> None:
        self._primary = primary
        self._fallback = fallback

    def evaluate(
        self,
        state: Mapping[str, Any],
        *,
        source_event_ns: int,
        ttl_ns: int,
    ) -> FailoverResult:
        try:
            observation = self._primary.evaluate(
                state, source_event_ns=source_event_ns, ttl_ns=ttl_ns
            )
            return FailoverResult(observation, "jev", False)
        except JevUnavailable:
            pass

        try:
            observation = self._fallback.evaluate(
                state, source_event_ns=source_event_ns, ttl_ns=ttl_ns
            )
        except LayaUnavailable as exc:
            raise JevUnavailable("all System-One decision providers unavailable") from exc
        return FailoverResult(observation, "laya", True)
