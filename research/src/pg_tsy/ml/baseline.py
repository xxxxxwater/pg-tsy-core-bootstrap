from __future__ import annotations

import numpy as np


def normalize_logits(logits: np.ndarray) -> np.ndarray:
    """Stable softmax used by tiny research baselines and contract tests."""
    logits = np.asarray(logits, dtype=np.float64)
    shifted = logits - np.max(logits, axis=-1, keepdims=True)
    exp = np.exp(shifted)
    return exp / np.sum(exp, axis=-1, keepdims=True)


def directional_score(prob_down: float, prob_flat: float, prob_up: float) -> float:
    total = prob_down + prob_flat + prob_up
    if total <= 0:
        raise ValueError("probabilities must sum to a positive number")
    return float((prob_up - prob_down) / total)
