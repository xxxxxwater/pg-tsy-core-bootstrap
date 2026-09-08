from __future__ import annotations

import argparse
from pathlib import Path

from .signal.models import Signal
from .signal.store import JsonlSignalStore


def _demo_signal(asset: str, out: str | None) -> int:
    signal = Signal.now(
        alpha_id="factor.vwap_deviation.v1",
        asset=asset,
        venue="BINANCE_PM",
        score=0.25,
        confidence=0.65,
        horizon_ms=60_000,
        feature_set="demo.v1",
        metadata={"mode": "development"},
    )
    if out:
        JsonlSignalStore(Path(out)).append(signal)
    print(signal.to_json())
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(prog="pg-tsy")
    sub = parser.add_subparsers(dest="command", required=True)
    demo = sub.add_parser("demo-signal")
    demo.add_argument("--asset", default="SOLUSDT")
    demo.add_argument("--out")
    args = parser.parse_args()
    if args.command == "demo-signal":
        return _demo_signal(args.asset, args.out)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
