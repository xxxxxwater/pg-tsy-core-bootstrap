"""Binary integration contract: run with PG_SIM_BINARY pointing at built pg-sim.

This test intentionally does not skip when the binary is absent: the dedicated
hardening CI must build the binary before declaring Python/Rust parity.
"""

import os
from pathlib import Path

from pg_tsy.sim import RustSimClient, SimRequest


ORDER = {
    "order_id": "f4c8c02e-5e3a-4076-8714-69137e7dcd2e",
    "side": "Buy",
    "kind": "Limit",
    "quantity": "2",
    "limit_price": "101",
    "time_in_force": "Ioc",
    "expire_at_ns": None,
    "post_only": False,
    "reduce_only": False,
    "display_quantity": None,
    "contingency": None,
}
TOP = {"bid_price": "99", "bid_quantity": "4", "ask_price": "100", "ask_quantity": "3"}


def _binary() -> Path:
    path = Path(
        os.environ.get(
            "PG_SIM_BINARY",
            str(Path(__file__).resolve().parents[2] / "rust" / "target" / "release" / "pg-sim"),
        )
    )
    assert path.is_file(), f"pg-sim executable missing: {path}"
    return path


def test_persistent_jsonl_bridge_is_deterministic_and_isolated() -> None:
    request = SimRequest(order=ORDER, top=TOP, position="0", now_ns=42)
    with RustSimClient(_binary()) as client:
        first = client.step(request)
        assert first["ok"] is True
        assert first["state"] == "Filled"
        assert first["filled_quantity"] == "2"
        assert first["fill"] == {"quantity": "2", "price": "100"}
        batch = client.batch([request] * 70)
        assert len(batch) == 70
        assert all(result == first for result in batch)


def test_reduce_only_and_book_errors_fail_closed() -> None:
    with RustSimClient(_binary()) as client:
        reduce_only = {**ORDER, "reduce_only": True}
        result = client.step(SimRequest(order=reduce_only, top=TOP, position="0"))
        assert result["ok"] is True
        assert result["outcome"] == "Rejected"
        assert result["filled_quantity"] == "0"

        crossed = {**TOP, "bid_price": "102"}
        rejected = client.step(SimRequest(order=ORDER, top=crossed, position="0"))
        assert rejected["ok"] is False
        assert "top of book" in rejected["error"]

        missing_order = client.step(SimRequest(order={"base": {"quantity": "2"}}, top=TOP))
        assert missing_order["ok"] is False
        assert "invalid order" in missing_order["error"]
