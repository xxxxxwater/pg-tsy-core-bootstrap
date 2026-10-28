"""Conservative RSS -> Coinbase LIVE signal validator.

No live orders here. Coinbase adapter is separated to make it impossible for
unauthenticated feed data to call the SDK directly.
"""
from dataclasses import dataclass
from decimal import Decimal
import re
import time

@dataclass(frozen=True)
class Signal:
    event_id: str
    product: str
    side: str
    rule: str
    score: float
    title: str
    source: str
    published: int

# Mirrors integrations/puregamma_rss/policy.py (_OFFICIAL, _UNCERTAIN). Kept as a
# copy, not an import: this sidecar's image only contains src/. Keep in sync.
# Stable RSS article source IDs (articles.source), never display names: a
# label such as "SecurityWeek" must not be treated as the SEC.
OFFICIAL_SOURCE_IDS = frozenset({'sec_press','sec_press_releases'})
BLOCK = re.compile(r'\b(?:not|no|fake|hoax|rumou?r|denies|denied|rejects?|rejected|proposes?|proposed|considering|may|might|could|alleged|plans?|draft|delays?|delayed|hacks?|warning|potential|unconfirmed)\b',re.I)
APPROVAL = re.compile(r'\b(?:approves?|approved|approval|authorized|authorizes?|authorised)\b',re.I)
BTC = re.compile(r'\b(?:bitcoin|btc)\b',re.I)
SPOT = re.compile(r'\bspot\b',re.I)
ETF = re.compile(r'\b(?:ETF|exchange[- ]traded fund)\b',re.I)


def make_signal(event, now=None, maximum_age=120):
    """Only direct SEC authoritative spot BTC ETF approvals; otherwise no trade.

    This is deliberately extremely selective: broad Fed rhetoric, CPI releases,
    exchange Telegram headlines, chat rumors or AI-translated copy are not directional.
    """
    now = int(now or time.time())
    if event.get('trust') != 'A' or event.get('priority') not in {'P0','P1'}:
        return None
    if str(event.get('source_id') or '') not in OFFICIAL_SOURCE_IDS:
        return None
    title = str(event.get('title',''))
    try:
        pub = int(event.get('published'))
    except (TypeError, ValueError):
        return None
    if now-pub<0 or now-pub>maximum_age:
        return None
    if BLOCK.search(title):
        return None
    if not (APPROVAL.search(title) and BTC.search(title) and SPOT.search(title) and ETF.search(title)):
        return None
    return Signal(str(event['id']), 'BTC-USD', 'BUY', 'sec_spot_btc_etf_approval_v1',0.8,title,str(event.get('source') or event['source_id']),pub)


def risk_allow(quote,limit,bid,ask,spread_bps=20):
    quote,limit,bid,ask=map(Decimal,map(str,(quote,limit,bid,ask)))
    if quote <= 0 or quote > limit or bid <= 0 or ask <= bid:
        return False
    if (ask-bid)/bid*Decimal(10000)>Decimal(str(spread_bps)):
        return False
    return True


def make_intent_id(event_id):
    import hashlib
    return 'pgrss-'+hashlib.sha256(('coinbase-live-v1:'+event_id).encode()).hexdigest()[:40]
