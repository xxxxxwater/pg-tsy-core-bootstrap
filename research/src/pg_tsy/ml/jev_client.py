"""Research-only JEV System One client. It NEVER submits orders.

This synchronous HTTP boundary is for shadow experiments and recording observations,
not the Rust market-data / cancel / emergency hot path. Inject a bounded transport
and clock in tests. No network retries: stale snapshots must not be re-evaluated.
"""

from __future__ import annotations

import json
import math
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Mapping

ENDPOINT = "https://api.typesafe.ai/v1/systemone"
MODEL = "jev-1.13.0"
OPTIONS = frozenset({"up", "down", "neutral"})
Transport = Callable[[bytes, str, float], bytes]


class JevUnavailable(ValueError):
    """Inference must not authorize new model-dependent exposure."""


@dataclass(frozen=True)
class JevObservation:
    instrument: str
    model: str
    source_event_ns: int
    received_ns: int
    expires_ns: int
    choice: str
    probabilities: dict[str, float]
    provider_confidence: float
    input_tokens: int
    output_tokens: int

    def usable_at(self, now_ns: int) -> bool:
        return self.received_ns <= now_ns < self.expires_ns


def _bounded_number(value: Any, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (float, int)):
        raise JevUnavailable(f"invalid {name}")
    number = float(value)
    if not math.isfinite(number) or not 0.0 <= number <= 1.0:
        raise JevUnavailable(f"invalid {name}")
    return number


def _http_transport(body: bytes, api_key: str, timeout_s: float) -> bytes:
    request = urllib.request.Request(
        ENDPOINT,
        data=body,
        headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
        method="POST",
    )
    # urllib follows redirects by default. Never put credentials in logs or exceptions.
    try:
        with urllib.request.urlopen(request, timeout=timeout_s) as response:
            if response.status != 200:
                raise JevUnavailable("non-200 inference response")
            return response.read(65537)
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise JevUnavailable("inference transport unavailable") from exc


class JevClient:
    def __init__(
        self,
        api_key: str | None = None,
        *,
        timeout_s: float = 0.35,
        transport: Transport = _http_transport,
        clock_ns: Callable[[], int] = time.time_ns,
    ) -> None:
        self._key = api_key if api_key is not None else os.environ.get("TYPESAFE_API_KEY", "")
        if not self._key or not 0 < timeout_s <= 10:
            raise ValueError("API key and a bounded positive timeout are required")
        self._timeout_s = timeout_s
        self._transport = transport
        self._clock_ns = clock_ns

    def evaluate(
        self,
        state: Mapping[str, Any],
        *,
        source_event_ns: int,
        ttl_ns: int,
    ) -> JevObservation:
        """One immutable market snapshot in, validated advisory observation out.

        `confidence` is provider certainty, NOT calibrated probability of BTC moving.
        Caller must retain the state and response for causal replay and audit.
        """
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
            raise JevUnavailable("invalid or stale BTCUSDC snapshot")
        expires = source_event_ns + ttl_ns
        request = {
            "model": MODEL,
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
        body = json.dumps(request, allow_nan=False, separators=(",", ":")).encode("utf-8")
        try:
            raw = self._transport(body, self._key, self._timeout_s)
            if len(raw) > 65536:
                raise JevUnavailable("oversized inference response")
            payload = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError, TypeError, OverflowError) as exc:
            raise JevUnavailable("malformed inference response") from exc
        received = self._clock_ns()
        if received >= expires or received < now:
            raise JevUnavailable("expired inference response")
        if not isinstance(payload, dict) or payload.get("model") != MODEL:
            raise JevUnavailable("unrecognized model version")
        answers = payload.get("answers")
        if not isinstance(answers, dict) or set(answers) != {"direction"}:
            raise JevUnavailable("incorrect answer schema")
        answer = answers["direction"]
        if not isinstance(answer, dict) or answer.get("type") != "choice":
            raise JevUnavailable("incorrect choice schema")
        probs = answer.get("probabilities")
        if not isinstance(probs, dict) or set(probs) != OPTIONS:
            raise JevUnavailable("incorrect choice distribution")
        clean = {option: _bounded_number(value, option) for option, value in probs.items()}
        if abs(sum(clean.values()) - 1.0) > 0.001:
            raise JevUnavailable("non-normalized probabilities")
        choice = answer.get("choice")
        if choice not in OPTIONS or clean[choice] < max(clean.values()) - 0.001:
            raise JevUnavailable("inconsistent winning choice")
        confidence = _bounded_number(answer.get("confidence"), "provider confidence")
        usage = payload.get("usage")
        if not isinstance(usage, dict):
            raise JevUnavailable("missing usage")
        tokens = (usage.get("input_tokens"), usage.get("output_tokens"))
        if any(not isinstance(x, int) or isinstance(x, bool) or x < 0 for x in tokens):
            raise JevUnavailable("invalid token usage")
        return JevObservation(
            instrument="BINANCE_PM:BTCUSDC",
            model=MODEL,
            source_event_ns=source_event_ns,
            received_ns=received,
            expires_ns=expires,
            choice=choice,
            probabilities=clean,
            provider_confidence=confidence,
            input_tokens=tokens[0],
            output_tokens=tokens[1],
        )
