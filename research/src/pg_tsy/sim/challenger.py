"""Paired, causal shadow challenger; no strategy writes or live orders.

A challenger is evaluated on the *same* tape, assumed queue, fees and latency.
Provider confidence is a gating feature, never treated as win probability.
"""

from __future__ import annotations

from dataclasses import dataclass
from decimal import Decimal
from math import ceil
from typing import Literal

from .causal import PassiveOrder, ReplayResult, TapeEvent, replay_quote

StrategyName = Literal["rules", "statistical", "jev", "jev_confidence"]


@dataclass(frozen=True, slots=True)
class Observation:
    """All scores must have been computed strictly before decision_ns."""

    case_id: str
    tape: tuple[TapeEvent, ...]
    order: PassiveOrder
    feature_available_ns: int
    statistical_score: float
    model_score: float | None
    model_confidence: float | None
    model_arrived_ns: int | None
    model_version: str | None

    def __post_init__(self) -> None:
        if not self.case_id or not self.tape:
            raise ValueError("case must have identity and immutable replay events")
        if self.feature_available_ns > self.order.decision_ns:
            raise ValueError("lookahead: feature arrived after policy decision")
        if not 0 <= self.statistical_score <= 1:
            raise ValueError("statistical score outside [0,1]")
        present = [self.model_score, self.model_confidence, self.model_arrived_ns, self.model_version]
        if any(value is None for value in present) and not all(value is None for value in present):
            raise ValueError("partial model evidence")
        if self.model_score is not None:
            if not (0 <= self.model_score <= 1 and 0 <= self.model_confidence <= 1):
                raise ValueError("invalid model scores")
            if self.model_arrived_ns < self.feature_available_ns:
                raise ValueError("model reply precedes feature availability")
            if not self.model_version:
                raise ValueError("model version must be pinned")


@dataclass(frozen=True, slots=True)
class ShadowDecision:
    case_id: str
    strategy: StrategyName
    accepted: bool
    result: ReplayResult | None
    reason: str


def compare_four(
    cases: list[Observation],
    *,
    queue: Literal["front", "back"],
    stat_threshold: float,
    model_threshold: float,
    confidence_threshold: float,
    model_max_age_ns: int,
    pinned_model: str,
) -> dict[StrategyName, tuple[ShadowDecision, ...]]:
    """Paired comparison; model latency is carried into real quote arrival.

    model_score means *risk/toxicity*, with lower values passing. Neither
    confidence nor model_score is a calibrated profit probability. Independent
    cases do not constitute portfolio PnL or a tradable strategy.
    """
    if not all(0 <= threshold <= 1 for threshold in
               (stat_threshold, model_threshold, confidence_threshold)):
        raise ValueError("thresholds outside [0,1]")
    if model_max_age_ns <= 0 or not pinned_model:
        raise ValueError("invalid TTL or unpinned model")
    if len({case.case_id for case in cases}) != len(cases):
        raise ValueError("duplicate cases")
    output: dict[StrategyName, list[ShadowDecision]] = {
        "rules": [], "statistical": [], "jev": [], "jev_confidence": []
    }
    for case in cases:
        if case.tape[0].ts_ns > case.order.decision_ns:
            raise ValueError("missing feature-time market snapshot")
        available_model = (case.model_arrived_ns is not None
                           and case.model_version == pinned_model
                           and case.model_arrived_ns >= case.order.decision_ns
                           and case.model_arrived_ns - case.order.decision_ns <= model_max_age_ns)
        for strategy, rows in output.items():
            accepted = strategy == "rules"
            reason = "baseline"
            if strategy == "statistical":
                accepted = case.statistical_score <= stat_threshold
                reason = "statistical_filter"
            elif strategy in ("jev", "jev_confidence"):
                accepted = bool(available_model and case.model_score <= model_threshold)
                reason = "model_filter" if available_model else "model_missing_stale_or_unpinned"
                if strategy == "jev_confidence":
                    accepted = bool(accepted and case.model_confidence >= confidence_threshold)
                    reason = "model_confidence_filter" if available_model else reason
            order = case.order
            if strategy in ("jev", "jev_confidence") and accepted:
                order = PassiveOrder(
                    side=order.side, price=order.price, quantity=order.quantity,
                    decision_ns=order.decision_ns,
                    inference_ready_ns=max(order.inference_ready_ns, case.model_arrived_ns),
                    outbound_latency_ns=order.outbound_latency_ns,
                    cancel_requested_ns=order.cancel_requested_ns,
                    cancel_latency_ns=order.cancel_latency_ns,
                    maker_fee_bps=order.maker_fee_bps, markout_ns=order.markout_ns,
                )
            result = replay_quote(list(case.tape), order, queue) if accepted else None
            rows.append(ShadowDecision(case.case_id, strategy, accepted, result, reason))
    return {name: tuple(items) for name, items in output.items()}


def net_by_strategy(
    decisions: dict[StrategyName, tuple[ShadowDecision, ...]],
) -> dict[StrategyName, Decimal]:
    """Exclude incomplete marks entirely; never turn unavailable marks into zero."""
    totals = {}
    for name, rows in decisions.items():
        if any(row.result is not None and not row.result.markout_complete for row in rows):
            raise ValueError(f"{name}: missing follow-up mark")
        totals[name] = sum((row.result.net_quote for row in rows if row.result is not None),
                           Decimal(0))
    return totals


def calibration(y: list[int], probability: list[float], bins: int = 10) -> tuple[float, float]:
    """Brier and ECE on held-out *event labels*, not vendor confidence."""
    if not y or len(y) != len(probability) or bins <= 0:
        raise ValueError("nonempty aligned labels/predictions required")
    if any(label not in (0, 1) for label in y):
        raise ValueError("labels must be binary")
    if any(not 0 <= p <= 1 for p in probability):
        raise ValueError("probabilities outside [0,1]")
    brier = sum((p - label) ** 2 for label, p in zip(y, probability)) / len(y)
    ece = 0.0
    for group in range(bins):
        indexes = [i for i, p in enumerate(probability)
                   if min(int(p * bins), bins - 1) == group]
        if indexes:
            frequency = sum(y[i] for i in indexes) / len(indexes)
            confidence = sum(probability[i] for i in indexes) / len(indexes)
            ece += len(indexes) / len(y) * abs(frequency - confidence)
    return brier, ece


def latency_quantiles(latencies_ns: list[int]) -> tuple[int, int]:
    if not latencies_ns or any(value < 0 for value in latencies_ns):
        raise ValueError("nonempty nonnegative latency measurements required")
    sorted_latency = sorted(latencies_ns)
    return sorted_latency[ceil(0.95 * len(sorted_latency)) - 1], sorted_latency[
        ceil(0.99 * len(sorted_latency)) - 1]
