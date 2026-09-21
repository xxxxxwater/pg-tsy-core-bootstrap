from __future__ import annotations

from dataclasses import dataclass
from enum import IntEnum

import numpy as np


class Action(IntEnum):
    HOLD = 0
    BUY = 1
    SELL = 2


@dataclass(slots=True)
class StepResult:
    observation: np.ndarray
    reward: np.ndarray
    terminated: np.ndarray


class BatchMarketEnv:
    """Vectorized research environment for RL/ES policy loops.

    Strategy authoring stays in Python. Exchange-accurate order semantics live in
    the Rust simulator; this NumPy path is intentionally a lightweight trainer.
    """

    def __init__(
        self,
        prices: np.ndarray,
        *,
        fee_bps: float = 0.0,
        max_position: float = 1.0,
    ) -> None:
        values = np.asarray(prices, dtype=np.float64)
        if values.ndim != 2 or values.shape[1] < 2:
            raise ValueError("prices must have shape [batch, time] with time >= 2")
        if np.any(~np.isfinite(values)) or np.any(values <= 0):
            raise ValueError("prices must be finite and positive")
        if max_position <= 0:
            raise ValueError("max_position must be positive")
        self._prices = values
        self._fee_rate = float(fee_bps) / 10_000.0
        self._max_position = float(max_position)
        self._batch = values.shape[0]
        self._cursor = 0
        self._position = np.zeros(self._batch, dtype=np.float64)

    @property
    def batch_size(self) -> int:
        return self._batch

    @property
    def cursor(self) -> int:
        return self._cursor

    def reset(self) -> np.ndarray:
        self._cursor = 0
        self._position.fill(0.0)
        return self._observation()

    def _observation(self) -> np.ndarray:
        px = self._prices[:, self._cursor]
        return np.column_stack((px, self._position))

    def step(self, actions: np.ndarray) -> StepResult:
        if self._cursor >= self._prices.shape[1] - 1:
            raise RuntimeError("episode already terminated")
        act = np.asarray(actions, dtype=np.int8)
        if act.shape != (self._batch,):
            raise ValueError(f"actions must have shape ({self._batch},)")
        if np.any((act < Action.HOLD) | (act > Action.SELL)):
            raise ValueError("actions contain an unknown action code")

        old_position = self._position.copy()
        target = np.where(
            act == Action.BUY,
            self._max_position,
            np.where(act == Action.SELL, -self._max_position, old_position),
        )
        turnover = np.abs(target - old_position)

        p0 = self._prices[:, self._cursor]
        self._cursor += 1
        p1 = self._prices[:, self._cursor]
        self._position = target

        gross = target * ((p1 / p0) - 1.0)
        costs = turnover * self._fee_rate
        reward = gross - costs
        done = self._cursor == self._prices.shape[1] - 1
        terminated = np.full(self._batch, done, dtype=np.bool_)
        return StepResult(self._observation(), reward, terminated)

    def rollout(self, policy) -> np.ndarray:
        """Run one complete vectorized episode and return per-environment rewards."""
        obs = self.reset()
        total = np.zeros(self._batch, dtype=np.float64)
        while self._cursor < self._prices.shape[1] - 1:
            actions = np.asarray(policy(obs), dtype=np.int8)
            result = self.step(actions)
            total += result.reward
            obs = result.observation
        return total
