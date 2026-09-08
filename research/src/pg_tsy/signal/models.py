from __future__ import annotations

import json
import time
from dataclasses import asdict, dataclass, field
from typing import Any, Literal
from uuid import uuid4

Venue = Literal["BINANCE_PM", "HYPERLIQUID"]


@dataclass(frozen=True, slots=True)
class Signal:
    alpha_id: str
    asset: str
    venue: Venue
    score: float
    confidence: float
    horizon_ms: int
    created_at_ns: int
    expires_at_ns: int
    signal_id: str = field(default_factory=lambda: str(uuid4()))
    schema_version: str = "signal.v1"
    model_version: str | None = None
    feature_set: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.schema_version != "signal.v1":
            raise ValueError("unsupported signal schema")
        if not -1.0 <= self.score <= 1.0:
            raise ValueError("score must be in [-1, 1]")
        if not 0.0 <= self.confidence <= 1.0:
            raise ValueError("confidence must be in [0, 1]")
        if self.horizon_ms <= 0:
            raise ValueError("horizon_ms must be positive")
        if self.expires_at_ns <= self.created_at_ns:
            raise ValueError("expires_at_ns must be after created_at_ns")

    @classmethod
    def now(
        cls,
        *,
        alpha_id: str,
        asset: str,
        venue: Venue,
        score: float,
        confidence: float,
        horizon_ms: int,
        model_version: str | None = None,
        feature_set: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> Signal:
        created = time.time_ns()
        return cls(
            alpha_id=alpha_id,
            asset=asset,
            venue=venue,
            score=score,
            confidence=confidence,
            horizon_ms=horizon_ms,
            created_at_ns=created,
            expires_at_ns=created + horizon_ms * 1_000_000,
            model_version=model_version,
            feature_set=feature_set,
            metadata=metadata or {},
        )

    def is_expired(self, now_ns: int | None = None) -> bool:
        return (now_ns or time.time_ns()) >= self.expires_at_ns

    def to_json(self) -> str:
        return json.dumps(asdict(self), separators=(",", ":"), sort_keys=True)
