"""Deterministic evidence gate. A pass NEVER sends orders or grants authorization."""

from __future__ import annotations

from dataclasses import dataclass
from decimal import Decimal
from typing import Literal

Stage = Literal["shadow", "paper", "canary"]


@dataclass(frozen=True, slots=True)
class Evidence:
    stage: Stage
    code_commit: str
    dataset_sha256: str
    fee_schedule_id: str
    model_version: str
    observations: int
    out_of_sample_windows: int
    no_lookahead: bool
    replay_calibrated_to_actual_fills: bool
    fill_history_complete: bool
    ownership_reconciled: bool
    no_duplicate_exposure_faults: bool
    user_stream_reconnect_verified: bool
    emergency_exit_verified: bool
    p95_latency_ms: float
    p99_latency_ms: float
    max_p99_ms: float
    brier: float
    ece: float
    max_ece: float
    realized_net_quote: Decimal
    baseline_net_quote: Decimal
    max_drawdown_quote: Decimal
    max_allowed_drawdown_quote: Decimal
    external_reviewer: str | None
    operator_canary_authorized: bool = False


@dataclass(frozen=True, slots=True)
class GateResult:
    eligible_for_review: bool
    blockers: tuple[str, ...]
    # No "deploy" output: operator approval and venue configuration stay external.


def review_gate(e: Evidence) -> GateResult:
    """Return blockers using independent evidence, never agent assertions alone."""
    reasons: list[str] = []
    if e.stage not in ("shadow", "paper", "canary"):
        reasons.append("unknown_stage")
    if len(e.code_commit) != 40 or any(c not in "0123456789abcdef" for c in e.code_commit):
        reasons.append("unverified_code_commit")
    if len(e.dataset_sha256) != 64 or any(c not in "0123456789abcdef" for c in e.dataset_sha256):
        reasons.append("missing_dataset_digest")
    if not e.fee_schedule_id or not e.model_version:
        reasons.append("missing_fee_or_model_provenance")
    if e.model_version.startswith("laya@unpinned:"):
        reasons.append("unpinned_laya_build")
    if e.observations < 1_000 or e.out_of_sample_windows < 3:
        reasons.append("insufficient_walk_forward_evidence")
    if not e.no_lookahead:
        reasons.append("lookahead_not_disproved")
    if not e.replay_calibrated_to_actual_fills:
        reasons.append("fill_model_not_calibrated")
    if not e.fill_history_complete or not e.ownership_reconciled:
        reasons.append("order_history_or_ownership_unknown")
    if not e.no_duplicate_exposure_faults:
        reasons.append("lost_ack_fault_injection_unproven")
    if not e.user_stream_reconnect_verified or not e.emergency_exit_verified:
        reasons.append("stream_or_emergency_unverified")
    if not (0 <= e.p95_latency_ms <= e.p99_latency_ms <= e.max_p99_ms):
        reasons.append("latency_budget_failed")
    if not (0 <= e.brier <= 1 and 0 <= e.ece <= e.max_ece <= 1):
        reasons.append("calibration_gate_failed")
    if e.realized_net_quote <= 0 or e.realized_net_quote <= e.baseline_net_quote:
        reasons.append("net_alpha_not_demonstrated")
    if not 0 <= e.max_drawdown_quote <= e.max_allowed_drawdown_quote:
        reasons.append("drawdown_gate_failed")
    if not e.external_reviewer or not e.external_reviewer.strip():
        reasons.append("independent_review_missing")
    if e.stage == "canary" and not e.operator_canary_authorized:
        reasons.append("explicit_canary_authorization_missing")
    return GateResult(not reasons, tuple(reasons))
