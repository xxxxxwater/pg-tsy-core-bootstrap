from __future__ import annotations

from collections.abc import Callable
from typing import Any


def optimize(
    objective: Callable[[Any], float],
    *,
    n_trials: int = 100,
    seed: int = 7,
) -> Any:
    """Run local-only Optuna optimization and return the best trial.

    The objective should itself perform chronological/purged out-of-sample validation.
    """
    try:
        import optuna
    except ImportError as exc:
        raise RuntimeError("install the local training extra: pip install -e '.[train]'") from exc

    sampler = optuna.samplers.TPESampler(seed=seed)
    study = optuna.create_study(direction="maximize", sampler=sampler)
    study.optimize(objective, n_trials=n_trials)
    return study.best_trial
