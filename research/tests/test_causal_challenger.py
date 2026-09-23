from dataclasses import replace
from decimal import Decimal as D

import pytest

from pg_tsy.sim.causal import (
    ActualFill,
    PassiveOrder,
    TapeEvent,
    compare_actual_fills,
    queue_sensitivity,
    replay_quote,
)
from pg_tsy.sim.challenger import (
    Observation,
    calibration,
    compare_four,
    latency_quantiles,
    net_by_strategy,
)
from pg_tsy.sim.release_gate import Evidence, review_gate


def tape(steps=7, *, bid=D(99), ask=D(101)):
    return [TapeEvent(i * 100, bid, ask, D(2), D(2), D(1)) for i in range(steps)]


def order(**kwargs):
    base = PassiveOrder("buy", D(99), D(2), 0, 0, 0, None, 0, D(1), 100)
    return replace(base, **kwargs)


def test_queue_uncertainty_and_actual_fill_bias():
    bounds = queue_sensitivity(tape(), order())
    front = bounds["front"]
    back = bounds["back"]
    assert front.status == "filled" and back.status == "filled"
    assert front.fills[0].time_ns < back.fills[0].time_ns
    assert len(front.fills) == len(back.fills) == 2
    assert front.markout_complete and back.markout_complete
    actual = [ActualFill("trade-1", 100, D(1), D(99), D("0.01"))]
    assert compare_actual_fills(actual, front)["quantity_error"] == D(1)
    with pytest.raises(ValueError, match="duplicate"):
        compare_actual_fills(actual * 2, front)


def test_inference_and_network_latency_cannot_steal_earlier_trades():
    delayed = replay_quote(tape(), order(inference_ready_ns=250, outbound_latency_ns=30), "front")
    assert delayed.fills[0].time_ns == 400
    assert replay_quote(tape(), order(inference_ready_ns=800), "front").status == "unobserved_arrival"


def test_cancel_ack_race_and_incomplete_future_marks_fail_closed():
    race = replay_quote(tape(), order(cancel_requested_ns=150, cancel_latency_ns=200), "front")
    assert [fill.time_ns for fill in race.fills] == [100, 200]
    early = replay_quote(tape(), order(cancel_requested_ns=150, cancel_latency_ns=0), "front")
    assert [fill.time_ns for fill in early.fills] == [100]
    missing = replay_quote(tape(3), order(markout_ns=300), "front")
    assert not missing.markout_complete
    with pytest.raises(ValueError, match="complete future"):
        compare_actual_fills([ActualFill("x", 100, D(1), D(99), D(0))], missing)


def test_invalid_crossed_book_and_post_only_rejection():
    with pytest.raises(ValueError, match="crossed"):
        TapeEvent(1, D(101), D(100), D(1), D(1))
    rejected = replay_quote(tape(), order(price=D(101)), "front")
    assert rejected.status == "post_only_rejected" and rejected.fills == ()
    with pytest.raises(ValueError, match="strictly increasing"):
        replay_quote(tape()[:1] * 2, order(), "front")
    with pytest.raises(ValueError, match="precede"):
        order(inference_ready_ns=-1)


def observations(*, score=0.2, confidence=0.9, model_time=0, version="jev-pinned"):
    return Observation("case-A", tuple(tape()), order(), 0, 0.2, score,
                       confidence, model_time, version)


def test_challenger_paired_tape_no_stale_or_unpinned_model():
    choices = compare_four([observations()], queue="front", stat_threshold=0.5,
                           model_threshold=0.5, confidence_threshold=0.8,
                           model_max_age_ns=100, pinned_model="jev-pinned")
    assert all(rows[0].accepted for rows in choices.values())
    assert len(set(net_by_strategy(choices).values())) == 1
    low = replace(observations(), model_confidence=0.3)
    choices = compare_four([low], queue="front", stat_threshold=0.5,
                           model_threshold=0.5, confidence_threshold=0.8,
                           model_max_age_ns=100, pinned_model="jev-pinned")
    assert choices["jev"][0].accepted and not choices["jev_confidence"][0].accepted
    late = replace(observations(), model_arrived_ns=350)
    choices = compare_four([late], queue="front", stat_threshold=0.5,
                           model_threshold=0.5, confidence_threshold=0.8,
                           model_max_age_ns=100, pinned_model="jev-pinned")
    assert not choices["jev"][0].accepted
    with pytest.raises(ValueError, match="duplicate cases"):
        compare_four([low, low], queue="front", stat_threshold=0.5,
                     model_threshold=0.5, confidence_threshold=0.8,
                     model_max_age_ns=100, pinned_model="jev-pinned")


def test_model_latency_changes_fill_availability():
    later = replace(observations(), model_arrived_ns=250)
    choices = compare_four([later], queue="front", stat_threshold=0.5,
                           model_threshold=0.5, confidence_threshold=0.8,
                           model_max_age_ns=300, pinned_model="jev-pinned")
    assert choices["rules"][0].result.fills[0].time_ns == 100
    assert choices["jev"][0].result.fills[0].time_ns == 400
    with pytest.raises(ValueError, match="missing follow-up mark"):
        net_by_strategy(compare_four([replace(later, order=order(markout_ns=500))], queue="front",
                                     stat_threshold=0.5, model_threshold=0.5,
                                     confidence_threshold=0.8, model_max_age_ns=300,
                                     pinned_model="jev-pinned"))


def test_calibration_and_latency_math():
    assert calibration([0, 1], [0, 1]) == (0, 0)
    assert calibration([0, 1], [1, 0])[0] == 1
    assert latency_quantiles(list(range(1, 101))) == (95, 99)
    with pytest.raises(ValueError):
        calibration([1], [float("nan")])


def good_evidence():
    return Evidence(
        stage="shadow", code_commit="a" * 40, dataset_sha256="b" * 64,
        fee_schedule_id="signed-fee-export-2026-09", model_version="jev-pinned",
        observations=10_000, out_of_sample_windows=5,
        no_lookahead=True, replay_calibrated_to_actual_fills=True,
        fill_history_complete=True, ownership_reconciled=True,
        no_duplicate_exposure_faults=True, user_stream_reconnect_verified=True,
        emergency_exit_verified=True, p95_latency_ms=45, p99_latency_ms=90,
        max_p99_ms=100, brier=0.12, ece=0.02, max_ece=0.05,
        realized_net_quote=D(11), baseline_net_quote=D(10),
        max_drawdown_quote=D(4), max_allowed_drawdown_quote=D(5),
        external_reviewer="independent-person", operator_canary_authorized=False,
    )


def test_release_gate_never_approves_canary_without_operator():
    assert review_gate(good_evidence()).eligible_for_review
    blocked = review_gate(replace(good_evidence(), stage="canary"))
    assert not blocked.eligible_for_review
    assert "explicit_canary_authorization_missing" in blocked.blockers
    assert review_gate(replace(good_evidence(), stage="canary",
                               operator_canary_authorized=True)).eligible_for_review


def test_release_gate_faults_costs_and_lookahead_are_fatal():
    blocked = review_gate(replace(good_evidence(), no_duplicate_exposure_faults=False,
                                  no_lookahead=False, realized_net_quote=D(-1),
                                  external_reviewer=None))
    assert set(blocked.blockers) >= {"lost_ack_fault_injection_unproven",
                                     "lookahead_not_disproved",
                                     "net_alpha_not_demonstrated",
                                     "independent_review_missing"}


def test_unpinned_laya_build_cannot_supply_promotion_evidence():
    blocked = review_gate(
        replace(good_evidence(), model_version="laya@unpinned:laya-rl-agent:typed-decisions")
    )
    assert not blocked.eligible_for_review
    assert "unpinned_laya_build" in blocked.blockers
