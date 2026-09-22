from pg_tsy.sim import SimRequest


def test_sim_request_is_debuggable_json_contract() -> None:
    request = SimRequest(
        order={"base": {"quantity": "2"}},
        top={"bid_price": "99", "bid_quantity": "4", "ask_price": "100", "ask_quantity": "3"},
        position="-1",
        now_ns=42,
    )
    assert request.payload(7) == {
        "request_id": 7,
        "order": {"base": {"quantity": "2"}},
        "top": {
            "bid_price": "99",
            "bid_quantity": "4",
            "ask_price": "100",
            "ask_quantity": "3",
        },
        "position": "-1",
        "now_ns": 42,
        "session": "CONTINUOUS",
    }
