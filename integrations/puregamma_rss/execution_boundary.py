"""Explicit Coinbase LIVE integration gate: zero order authority in this module.

This is a contract checker, not a trading adapter. The actual executor must
be implemented inside TSY's pg-risk -> journal -> fenced OMS -> reconciliation
pipeline. No code here sends orders or reads private keys.
"""
from dataclasses import dataclass


@dataclass(frozen=True)
class LiveReadiness:
    risk_approved: bool = False
    ownership_verified: bool = False
    journal_fenced: bool = False
    portfolio_isolated: bool = False
    quote_fresh: bool = False
    fees_and_spread_checked: bool = False
    venue_order_recovery_verified: bool = False
    attached_protection_verified: bool = False
    human_live_approval: bool = False


def missing_live_gates(readiness: LiveReadiness) -> tuple[str, ...]:
    """Enumerate blockers; this is NEVER an order authorization token."""
    return tuple(
        field for field in readiness.__dataclass_fields__
        if not getattr(readiness, field)
    )


class CoinbaseExecutionNotRegistered(RuntimeError):
    pass


def dispatch_order(*_args, **_kwargs):
    """Hard-stop until a native, reviewed TSY adapter owns dispatch/recovery."""
    raise CoinbaseExecutionNotRegistered(
        "Coinbase LIVE orders are not registered in pg-core; "
        "review candidate has no execution authority"
    )
