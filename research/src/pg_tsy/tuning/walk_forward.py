from __future__ import annotations

from dataclasses import dataclass

import numpy as np


@dataclass(frozen=True, slots=True)
class WalkForwardFold:
    train_start: int
    train_end: int
    test_start: int
    test_end: int


def purged_walk_forward(
    n_samples: int,
    *,
    train_size: int,
    test_size: int,
    purge_size: int = 0,
    step_size: int | None = None,
) -> list[WalkForwardFold]:
    if min(n_samples, train_size, test_size) <= 0 or purge_size < 0:
        raise ValueError("invalid split sizes")
    step = step_size or test_size
    folds: list[WalkForwardFold] = []
    train_start = 0
    while True:
        train_end = train_start + train_size
        test_start = train_end + purge_size
        test_end = test_start + test_size
        if test_end > n_samples:
            break
        folds.append(WalkForwardFold(train_start, train_end, test_start, test_end))
        train_start += step
    return folds


def robustness_score(scores: list[float], *, dispersion_penalty: float = 0.5) -> float:
    if not scores:
        raise ValueError("at least one out-of-sample score is required")
    values = np.asarray(scores, dtype=np.float64)
    return float(np.median(values) - dispersion_penalty * np.std(values))
