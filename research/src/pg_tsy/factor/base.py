from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

import polars as pl


@dataclass(frozen=True, slots=True)
class FactorDefinition:
    factor_id: str
    version: int
    description: str
    expression: Callable[[], pl.Expr]

    @property
    def qualified_id(self) -> str:
        return f"{self.factor_id}.v{self.version}"


class FactorRegistry:
    def __init__(self) -> None:
        self._items: dict[str, FactorDefinition] = {}

    def register(self, factor: FactorDefinition) -> None:
        key = factor.qualified_id
        if key in self._items:
            raise ValueError(f"factor already registered: {key}")
        self._items[key] = factor

    def get(self, qualified_id: str) -> FactorDefinition:
        return self._items[qualified_id]

    def list(self) -> list[str]:
        return sorted(self._items)
