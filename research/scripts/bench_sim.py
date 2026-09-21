from __future__ import annotations

import argparse
import time

import numpy as np

from pg_tsy.sim import Action, BatchMarketEnv


def main() -> int:
    parser = argparse.ArgumentParser(description="Vectorized RL/ES environment throughput smoke")
    parser.add_argument("--batch", type=int, default=4096)
    parser.add_argument("--steps", type=int, default=2000)
    args = parser.parse_args()

    prices = np.linspace(100.0, 110.0, args.steps + 1, dtype=np.float64)
    matrix = np.broadcast_to(prices, (args.batch, prices.size)).copy()
    env = BatchMarketEnv(matrix)
    actions = np.full(args.batch, Action.BUY, dtype=np.int8)

    env.reset()
    started = time.perf_counter()
    interactions = 0
    for _ in range(args.steps):
        env.step(actions)
        interactions += args.batch
    elapsed = time.perf_counter() - started
    print(
        f"batch={args.batch} steps={args.steps} interactions={interactions} "
        f"elapsed_s={elapsed:.6f} interactions_per_second={interactions / elapsed:.0f}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
