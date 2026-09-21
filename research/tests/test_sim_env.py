from __future__ import annotations

import numpy as np
import pytest

from pg_tsy.sim import Action, BatchMarketEnv


def test_batch_env_rewards_follow_position_and_price_move() -> None:
    prices = np.array([[100.0, 110.0, 121.0], [100.0, 90.0, 81.0]])
    env = BatchMarketEnv(prices)
    obs = env.reset()
    assert obs.shape == (2, 2)

    first = env.step(np.array([Action.BUY, Action.SELL]))
    np.testing.assert_allclose(first.reward, np.array([0.1, 0.1]))
    second = env.step(np.array([Action.BUY, Action.SELL]))
    np.testing.assert_allclose(second.reward, np.array([0.1, 0.1]))
    assert second.terminated.all()


def test_fee_is_charged_on_turnover() -> None:
    env = BatchMarketEnv(np.array([[100.0, 100.0]]), fee_bps=10.0)
    result = env.step(np.array([Action.BUY]))
    np.testing.assert_allclose(result.reward, np.array([-0.001]))


def test_rollout_runs_vectorized_episode() -> None:
    prices = np.array([[100.0, 101.0, 102.0], [100.0, 99.0, 98.0]])
    env = BatchMarketEnv(prices)

    def policy(obs: np.ndarray) -> np.ndarray:
        return np.where(obs[:, 0] >= 100.0, Action.BUY, Action.SELL)

    total = env.rollout(policy)
    assert total.shape == (2,)
    assert np.isfinite(total).all()


def test_invalid_shape_fails_fast() -> None:
    with pytest.raises(ValueError):
        BatchMarketEnv(np.array([100.0, 101.0]))
