# PureGamma RSS V4: read-only TSY integration and Telegram monitoring

**Scope:** existing InstaBox RSS V4 + PG-TSY health monitoring and
**non-executable** event review. No Coinbase SDK, private trading keys,
order submission, or configuration change to the TSY daemon.

## System boundary

- RSS V4: official feeds, RSSHub, deduplication, raw article SQLite,
  event priority, Telegram Chinese translation using local LibreTranslate.
- PG-TSY: independent Rust engine, health endpoint, risk/OMS adapter registry.
- This bridge: read the original RSS `events` and `articles` tables through
  SQLite read-only URI mode; check RSS `/status` and TSY `/healthz`;
  persist seen fingerprints, advisory candidates and Telegram alert outbox.
- Telegram: send hourly heartbeat, three-consecutive-failure warning, recovery,
  and review candidate alerts. No trading commands accepted.

The Chinese translation is not input to candidate matching. Trust A requires
`articles.source = sec_press` and a recent affirmative original English SEC
spot BTC ETF approval title, not an aggregator nickname. An advisory candidate
contains product BTC-USD, original title and a deterministic rule ID but **no
side, size, limit price, or trading authorization**. Its disposition is
`REVIEW_REQUIRED` and it cannot reach an exchange.

WatcherGuruChat is a Telegram discussion group: its previous RSSHub 503
was a source-access failure, not proof of live access. Group discussions never
gain authority to generate official-event candidates.

## Deployment on the existing InstaBox host

This Compose file is an **additional container**, not a replacement for either
PureGamma RSS V4 or PG-TSY. Requires Linux host networking because the two
existing health endpoints listen on loopback. It opens no new inbound ports.

1. On the host, check `curl -f http://127.0.0.1:8765/status` and
   `curl -f http://127.0.0.1:8080/healthz`.
2. Enter `integrations/puregamma_rss` in an existing checked-out repository.
3. Copy `.env.example` to `.env` (do not commit) and set:
   `RSS_DATA_DIR` to the **existing** RSS V4 directory containing `events.db`,
   `TELEGRAM_TOKEN_PATH` to the existing Bot Token **file path** (not token
   contents), and `TELEGRAM_CHAT_ID` to the intended recipient.
4. Run:

~~~sh
mkdir -p state
chmod 700 state
chmod 600 .env
docker compose -f compose.monitoring.yml config -q
docker compose -f compose.monitoring.yml up -d
docker compose -f compose.monitoring.yml ps
docker compose -f compose.monitoring.yml logs --tail=60 monitor
~~~

The monitor's file mounts are read-only for the original events and token;
only its private local `state` directory is writable. First run **baselines**
previous RSS records and never replays old headlines as new signals.
Telegram delivery retries are persisted; an ambiguous Telegram timeout can
create a duplicate alert (at-least-once semantics).

## Validation and rollback

~~~sh
python3 -m unittest discover -s integrations/puregamma_rss/tests -v
# To stop this monitor without affecting TSY or RSSHub:
docker compose -f compose.monitoring.yml down
~~~

Check RSS freshness and TSY feed/ready states and actually receive a
heartbeat in Telegram. Healthy endpoints do not establish live Coinbase
order, fill or stop protection acceptance.

**Operational limit:** if InstaBox suspends or loses power, on-host heartbeat
also stops. Use an off-host watcher for machine-down alerts, and disable
auto-suspend only where account permissions allow it. Keep all credentials
and persistent state out of the repository.

Design details: [Coinbase LIVE prerequisites](../../docs/PUREGAMMA_RSS_COINBASE_LIVE.md).
