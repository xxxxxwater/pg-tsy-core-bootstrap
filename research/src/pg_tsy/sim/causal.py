"""Causal *research* replay for one isolated passive quote, never a venue emulator.

All observations are available only at their event timestamps. Public L2 cannot
reveal own queue rank; the front/back outputs are bounds, not observed fills.
No production order routing exists in this module.
"""

from __future__ import annotations

from dataclasses import dataclass
from decimal import Decimal
from itertools import pairwise
from typing import Literal

Side = Literal["buy", "sell"]
Queue = Literal["front", "back"]


@dataclass(frozen=True, slots=True)
class TapeEvent:
    ts_ns: int
    bid: Decimal
    ask: Decimal
    bid_size: Decimal
    ask_size: Decimal
    sells_at_bid: Decimal = Decimal(0)
    buys_at_ask: Decimal = Decimal(0)

    def __post_init__(self) -> None:
        if self.ts_ns < 0 or self.bid <= 0 or self.ask <= self.bid:
            raise ValueError("event time or prices invalid/crossed")
        if min(self.bid_size, self.ask_size, self.sells_at_bid, self.buys_at_ask) < 0:
            raise ValueError("sizes and trade volumes must be nonnegative")
        if not all(x.is_finite() for x in (self.bid, self.ask, self.bid_size,
                                            self.ask_size, self.sells_at_bid,
                                            self.buys_at_ask)):
            raise ValueError("non-finite market observation")

    @property
    def mid(self) -> Decimal:
        return (self.bid + self.ask) / 2


@dataclass(frozen=True, slots=True)
class PassiveOrder:
    side: Side
    price: Decimal
    quantity: Decimal
    decision_ns: int
    inference_ready_ns: int
    outbound_latency_ns: int
    cancel_requested_ns: int | None
    cancel_latency_ns: int
    maker_fee_bps: Decimal
    markout_ns: int

    def __post_init__(self) -> None:
        if self.side not in ("buy", "sell") or self.price <= 0 or self.quantity <= 0:
            raise ValueError("invalid order")
        if not self.price.is_finite() or not self.quantity.is_finite():
            raise ValueError("non-finite order")
        if self.decision_ns < 0 or self.inference_ready_ns < self.decision_ns:
            raise ValueError("model decision cannot precede the information timestamp")
        if min(self.outbound_latency_ns, self.cancel_latency_ns, self.markout_ns) < 0:
            raise ValueError("negative latency/horizon")
        if self.cancel_requested_ns is not None and self.cancel_requested_ns < self.decision_ns:
            raise ValueError("cancel before decision")
        if not self.maker_fee_bps.is_finite():
            raise ValueError("non-finite maker fee")

    @property
    def arrival_ns(self) -> int:
        return self.inference_ready_ns + self.outbound_latency_ns

    @property
    def cancel_effective_ns(self) -> int | None:
        if self.cancel_requested_ns is None:
            return None
        return self.cancel_requested_ns + self.cancel_latency_ns


@dataclass(frozen=True, slots=True)
class ReplayFill:
    time_ns: int
    quantity: Decimal
    price: Decimal
    fee_quote: Decimal
    markout_quote: Decimal

    @property
    def net_quote(self) -> Decimal:
        return self.markout_quote - self.fee_quote


@dataclass(frozen=True, slots=True)
class ReplayResult:
    queue: Queue
    status: str
    fills: tuple[ReplayFill, ...]
    unfilled: Decimal
    markout_complete: bool

    @property
    def net_quote(self) -> Decimal:
        return sum((fill.net_quote for fill in self.fills), Decimal(0))


def replay_quote(tape: list[TapeEvent], order: PassiveOrder, queue: Queue) -> ReplayResult:
    """Replay one quote with strict arrival/cancel ordering and bounded queue rank.

    A trade at exactly arrival/cancel timestamp is ambiguous; we conservatively
    exclude the arrival trade and exclude fills at/after cancel effectiveness.
    A fill is counted only after observed opposing traded volume depletes the
    assumed quantity ahead at the *same* touch. No simulated fill from L2 size
    shrinking alone, since public size declines could be cancels ahead/behind.
    Missing future marks make the result ineligible for profitability claims.
    """
    if queue not in ("front", "back") or not tape:
        raise ValueError("queue scenario must be front/back and tape nonempty")
    if any(a.ts_ns >= b.ts_ns for a, b in pairwise(tape)):
        raise ValueError("tape events must have strictly increasing timestamps")
    arrival = next((i for i, event in enumerate(tape) if event.ts_ns >= order.arrival_ns), None)
    if arrival is None:
        return ReplayResult(queue, "unobserved_arrival", (), order.quantity, True)
    initial = tape[arrival]
    if order.side == "buy":
        crosses = order.price >= initial.ask
        at_touch = order.price == initial.bid
        depth = initial.bid_size
    else:
        crosses = order.price <= initial.bid
        at_touch = order.price == initial.ask
        depth = initial.ask_size
    if crosses:
        return ReplayResult(queue, "post_only_rejected", (), order.quantity, True)
    if not at_touch:
        return ReplayResult(queue, "unsupported_off_touch", (), order.quantity, True)
    ahead = depth if queue == "back" else Decimal(0)
    remaining = order.quantity
    raw: list[tuple[int, Decimal, Decimal]] = []
    cancel_at = order.cancel_effective_ns
    for event in tape[arrival + 1:]:
        if cancel_at is not None and event.ts_ns >= cancel_at:
            break
        if event.ts_ns <= order.arrival_ns:
            continue
        if order.side == "buy":
            volume = event.sells_at_bid if event.bid == order.price else Decimal(0)
        else:
            volume = event.buys_at_ask if event.ask == order.price else Decimal(0)
        if volume <= 0:
            continue
        consumed_ahead = min(ahead, volume)
        ahead -= consumed_ahead
        fill_qty = min(remaining, volume - consumed_ahead)
        if fill_qty > 0:
            raw.append((event.ts_ns, fill_qty, order.price))
            remaining -= fill_qty
        if remaining == 0:
            break
    fills = []
    complete = True
    for ts, quantity, price in raw:
        mark = next((e.mid for e in tape if e.ts_ns > ts and e.ts_ns >= ts + order.markout_ns), None)
        if mark is None:
            complete = False
            continue
        signed = Decimal(1) if order.side == "buy" else Decimal(-1)
        pnl = signed * (mark - price) * quantity
        fee = price * quantity * order.maker_fee_bps / Decimal(10_000)
        fills.append(ReplayFill(ts, quantity, price, fee, pnl))
    status = "filled" if remaining == 0 else ("canceled" if cancel_at is not None else "resting")
    return ReplayResult(queue, status, tuple(fills), remaining, complete)


def queue_sensitivity(tape: list[TapeEvent], order: PassiveOrder) -> dict[Queue, ReplayResult]:
    """Return separate optimistic/pessimistic cases; never average invented ranks."""
    return {"front": replay_quote(tape, order, "front"),
            "back": replay_quote(tape, order, "back")}


@dataclass(frozen=True, slots=True)
class ActualFill:
    trade_id: str
    ts_ns: int
    quantity: Decimal
    price: Decimal
    commission_quote: Decimal


def compare_actual_fills(actual: list[ActualFill], simulated: ReplayResult) -> dict[str, Decimal]:
    """Compare *provided* authenticated fills; absence of actual data is an error.

    Fee assets must have been converted to quote currency with a verified FX
    mark before constructing ActualFill. The comparison is diagnostic only.
    """
    if not actual or not simulated.markout_complete:
        raise ValueError("actual fills and complete future marks are required")
    if len({fill.trade_id for fill in actual}) != len(actual):
        raise ValueError("duplicate real trade IDs")
    for fill in actual:
        if fill.quantity <= 0 or fill.price <= 0 or fill.commission_quote < 0:
            raise ValueError("invalid real fill")
    real_qty = sum((fill.quantity for fill in actual), Decimal(0))
    sim_qty = sum((fill.quantity for fill in simulated.fills), Decimal(0))
    real_fees = sum((fill.commission_quote for fill in actual), Decimal(0))
    sim_fees = sum((fill.fee_quote for fill in simulated.fills), Decimal(0))
    return {"quantity_error": sim_qty - real_qty,
            "fee_error_quote": sim_fees - real_fees,
            "real_quantity": real_qty,
            "sim_quantity": sim_qty}
