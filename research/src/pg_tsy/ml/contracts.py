from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True, slots=True)
class FeatureSpec:
    name: str
    dtype: str


@dataclass(frozen=True, slots=True)
class ModelManifest:
    model_id: str
    version: str
    framework: str
    feature_set: str
    features: Sequence[FeatureSpec]
    artifact_path: Path
    horizon_ms: int

    def validate(self) -> None:
        if self.framework not in {"pytorch", "jax", "onnx"}:
            raise ValueError(f"unsupported framework: {self.framework}")
        if self.horizon_ms <= 0:
            raise ValueError("horizon_ms must be positive")
