"""Deterministic, advisory-only PureGamma RSS event classifier.

No Coinbase SDK, network calls, credentials, or order endpoints may be imported
here. English ORIGINAL title is authoritative; translated text is display only.
"""
from dataclasses import dataclass
import re
import time

_OFFICIAL = frozenset({"sec_press", "sec_press_releases"})
_APPROVED = re.compile(r"\b(?:approves?|approved|authorized|authorised)\b", re.I)
_ASSET = re.compile(r"\b(?:bitcoin|btc)\b", re.I)
_SPOT = re.compile(r"\bspot\b", re.I)
_ETF = re.compile(r"\b(?:etf|exchange.traded fund)\b", re.I)
_UNCERTAIN = re.compile(
    r"\b(?:may|might|could|rumou?r|alleged|denies?|denied|"
    r"rejects?|rejected|proposes?|proposed|draft|considering|"
    r"not|unconfirmed|fake|hoax)\b", re.I
)


@dataclass(frozen=True)
class ReviewCandidate:
    event_id: str
    source_id: str
    product_id: str
    rule_id: str
    original_title: str
    first_seen: int
    disposition: str = "REVIEW_REQUIRED"


def candidate_from_event(event, now=None, max_age_seconds=120):
    """Return a NON-EXECUTABLE review candidate, or None.

    This deliberately cannot produce an order side, size, or permission.
    Validation is conservative; real Coinbase trading must route via pg-risk,
    durable execution fencing, venue identity, and full reconciliation.
    """
    now = int(time.time()) if now is None else int(now)
    source_id = str(event.get("source_id", ""))
    title = str(event.get("title", ""))
    timestamp = event.get("published")
    try:
        timestamp = int(timestamp)
    except (TypeError, ValueError):
        return None
    if source_id not in _OFFICIAL:
        return None
    if event.get("trust") != "A" or event.get("priority") not in ("P0", "P1"):
        return None
    if not 0 <= now - timestamp <= max_age_seconds:
        return None
    if len(title) < 25 or len(title) > 700 or _UNCERTAIN.search(title):
        return None
    if not all(pattern.search(title) for pattern in (_APPROVED, _ASSET, _SPOT, _ETF)):
        return None
    event_id = str(event.get("id", ""))
    if not event_id or len(event_id) > 128:
        return None
    return ReviewCandidate(
        event_id=event_id,
        source_id=source_id,
        product_id="BTC-USD",
        rule_id="sec_spot_btc_etf_confirmed_v1",
        original_title=title,
        first_seen=now,
    )
