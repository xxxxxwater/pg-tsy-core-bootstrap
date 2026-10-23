"""Independent read-only RSS + TSY health watcher with Telegram notification outbox.

Run this in a separate container from pg-core. This module NEVER calls any
exchange or private trading API. It only reads RSS SQLite and two health APIs.
"""
import json
import logging
import os
import sqlite3
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from policy import candidate_from_event

LOG = logging.getLogger("pg-rss-monitor")
RSS_DB = os.getenv("RSS_EVENTS_DB", "/rss/events.db")
STATE_DB = os.getenv("MONITOR_STATE_DB", "/state/monitor.sqlite")
RSS_STATUS = os.getenv("RSS_STATUS_URL", "http://127.0.0.1:8765/status")
TSY_STATUS = os.getenv("TSY_STATUS_URL", "http://127.0.0.1:8080/healthz")
TOKEN_FILE = os.getenv("TELEGRAM_TOKEN_FILE", "/run/secrets/bot_token")
CHAT_ID = os.getenv("TELEGRAM_CHAT_ID", "")
HEARTBEAT_SECONDS = max(600, int(os.getenv("HEARTBEAT_SECONDS", "3600")))
CYCLE_SECONDS = max(30, int(os.getenv("CYCLE_SECONDS", "60")))


def open_state(path=STATE_DB):
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(path, timeout=20)
    con.row_factory = sqlite3.Row
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("PRAGMA synchronous=FULL")
    con.executescript("""
        CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS seen (
            fingerprint TEXT PRIMARY KEY, observed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS candidates (
            event_id TEXT PRIMARY KEY, observed_at INTEGER, rule_id TEXT, title TEXT
        );
        CREATE TABLE IF NOT EXISTS outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,
            message TEXT NOT NULL, due_at INTEGER NOT NULL,
            sent_at INTEGER, attempts INTEGER NOT NULL DEFAULT 0
        );
    """)
    con.commit()
    return con


def kv_get(con, key, default=""):
    row = con.execute("SELECT v FROM kv WHERE k=?", (key,)).fetchone()
    return row["v"] if row else default


def kv_set(con, key, val):
    con.execute(
        "INSERT INTO kv(k,v) VALUES (?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        (key, str(val)),
    )
    con.commit()


def enqueue(con, kind, message, now):
    con.execute(
        "INSERT INTO outbox(kind,message,due_at) VALUES (?,?,?)",
        (kind, message[:3500], now),
    )
    con.commit()


def new_feed_candidates(con, events_db=RSS_DB, now=None):
    """First start establishes a baseline; never trades/replays old RSS."""
    now = int(time.time()) if now is None else int(now)
    # RSS database is mounted read-only; PRAGMA query_only forbids writes.
    uri = "file:" + urllib.parse.quote(str(Path(events_db).resolve())) + "?mode=ro"
    source = sqlite3.connect(uri, uri=True, timeout=5)
    source.row_factory = sqlite3.Row
    source.execute("PRAGMA query_only=ON")
    try:
        existing = source.execute(
            "SELECT e.id,e.title,e.source,e.priority,e.trust,e.published,e.discovered,e.last_seen, "
            "(SELECT a.source FROM articles a WHERE a.event_id=e.id "
            "AND a.source='sec_press' LIMIT 1) AS official_source_id "
            "FROM events e WHERE e.last_seen >= ? "
            "ORDER BY e.last_seen ASC LIMIT 500",
            (now - 300,),
        ).fetchall()
        if kv_get(con, "initialized") != "true":
            for row in existing:
                con.execute(
                    "INSERT OR IGNORE INTO seen VALUES (?,?)",
                    (row["id"] + ":" + row["priority"] + ":" + row["trust"], now),
                )
            kv_set(con, "initialized", "true")
            con.commit()
            return 0
        count = 0
        for row in existing:
            fingerprint = row["id"] + ":" + row["priority"] + ":" + row["trust"]
            if con.execute("SELECT 1 FROM seen WHERE fingerprint=?", (fingerprint,)).fetchone():
                continue
            con.execute("INSERT INTO seen VALUES (?,?)", (fingerprint, now))
            con.commit()
            # Filter to *newly discovered* stories; subsequent poll refreshes and
            # non-official source labels must not create a retroactive candidate.
            if not row["discovered"] or int(row["discovered"]) < now - 130:
                continue
            # Event source names are display labels, not stable IDs.
            # Explicit whitelist avoids treating community sources as official.
            source_id = row["official_source_id"] or ""
            event = dict(row)
            event["source_id"] = source_id
            c = candidate_from_event(event, now=now)
            if c:
                con.execute(
                    "INSERT OR IGNORE INTO candidates VALUES (?,?,?,?)",
                    (c.event_id, now, c.rule_id, c.original_title),
                )
                if con.execute("SELECT changes()").fetchone()[0]:
                    enqueue(
                        con, "candidate",
                        "📰 事件审核候选（不可自动下单）\n"
                        + c.original_title[:300]
                        + "\n产品：BTC-USD\n来源：SEC\n规则：" + c.rule_id
                        + "\n执行状态：REVIEW_REQUIRED / 无 Coinbase 下单权限",
                        now,
                    )
                    count += 1
        con.commit()
        return count
    finally:
        source.close()


def get_health(url):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=6) as resp:
        if resp.status != 200:
            raise ValueError("health status not OK")
        return json.load(resp)


def check_health(now):
    """Return deterministic health faults and metadata, without credentials."""
    problems = []
    parts = []
    try:
        rss = get_health(RSS_STATUS)
        if not rss.get("ok"):
            problems.append("RSS V4 not OK")
        parts.append(
            "RSS: events=%s pending=%s sent=%s"
            % (rss.get("events", "?"), rss.get("pending", "?"), rss.get("sent", "?"))
        )
        for source in rss.get("sources", []):
            if source.get("enabled") is False:
                continue
            name = str(source.get("id", "?"))[:60]
            if name == "watcherguru_chat" and not source.get("seeded"):
                # Known permission-dependent chat reader is not a liveness
                # check for the other official feeds.
                continue
            last_ok = source.get("last_ok")
            limit = 4200 if name == "rsshub_releases" else 1200
            if last_ok is None or now - int(last_ok) > limit:
                problems.append("RSS stale: " + name)
    except (OSError, ValueError, KeyError, TypeError):
        problems.append("RSS V4 health endpoint unavailable")

    try:
        tsy = get_health(TSY_STATUS)
        if not tsy.get("ready", False):
            problems.append("TSY not ready")
        connected, total = tsy.get("feeds_connected"), tsy.get("feeds_total")
        if connected is None or total is None or total < 1 or connected < total:
            problems.append("TSY market-data feeds degraded")
        parts.append(
            "TSY: mode=%s feeds=%s/%s"
            % (tsy.get("mode", "?"), connected, total)
        )
    except (OSError, ValueError, KeyError, TypeError):
        problems.append("TSY health endpoint unavailable")
    return sorted(set(problems)), parts


def send_message(token, chat_id, message):
    """Telegram sendMessage only; never expose token-bearing URL in exceptions."""
    payload = urllib.parse.urlencode(
        {"chat_id": chat_id, "text": message, "disable_web_page_preview": "true"}
    ).encode()
    req = urllib.request.Request(
        "https://api.telegram.org/bot" + token + "/sendMessage",
        data=payload, method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            data = json.load(response)
            return bool(data.get("ok")), 0
    except urllib.error.HTTPError as exc:
        retry = 60
        if exc.code == 429:
            try:
                retry = max(60, int(json.load(exc).get("parameters", {}).get("retry_after", 60)))
            except (ValueError, TypeError, KeyError):
                retry = 60
        return False, retry
    except Exception:
        # Failure may occur AFTER delivery: at-least-once, no exact-once claim.
        return False, 90


def flush_outbox(con, token, chat_id, now):
    sent = 0
    for row in con.execute(
        "SELECT id,message,attempts FROM outbox WHERE sent_at IS NULL AND due_at<=?"
        " ORDER BY id LIMIT 10", (now,)
    ).fetchall():
        ok, delay = send_message(token, chat_id, row["message"])
        if ok:
            con.execute("UPDATE outbox SET sent_at=? WHERE id=?", (now, row["id"]))
            sent += 1
            con.commit()
            time.sleep(1.1)
        else:
            backoff = max(delay, min(1800, 2 ** min(row["attempts"] + 1, 10)))
            con.execute(
                "UPDATE outbox SET attempts=attempts+1,due_at=? WHERE id=?",
                (now + backoff, row["id"]),
            )
            con.commit()
            break
    return sent


def tick(con, now):
    try:
        new_feed_candidates(con, now=now)
        kv_set(con, "rss_last_read_ok", now)
    except (OSError, sqlite3.Error, ValueError):
        # No accidental trades if the DB is unavailable.
        LOG.warning("RSS database read failed (no exchange side effects)")
    problems, parts = check_health(now)
    signature = json.dumps(problems, ensure_ascii=False)
    previous = kv_get(con, "health_problems", "[]")
    repeats = int(kv_get(con, "consecutive_health_failures", 0))
    repeats = repeats + 1 if problems else 0
    kv_set(con, "consecutive_health_failures", repeats)
    if problems and repeats >= 3 and signature != previous:
        enqueue(con, "alert", "🚨 健康告警\n" + "\n".join(problems), now)
        kv_set(con, "health_problems", signature)
    elif not problems and previous != "[]":
        enqueue(con, "recovery", "✅ RSS / TSY 已恢复\n" + "\n".join(parts), now)
        kv_set(con, "health_problems", "[]")
    last = int(kv_get(con, "last_heartbeat", 0))
    if now - last >= HEARTBEAT_SECONDS:
        enqueue(
            con, "heartbeat",
            "💓 PureGamma RSS V4 + PG-TSY 心跳\n"
            + "\n".join(parts)
            + ("\n告警：" + ", ".join(problems) if problems else "\n检查结果：正常")
            + "\nCoinbase LIVE：未在本监控服务注册执行权限",
            now,
        )
        kv_set(con, "last_heartbeat", now)


def main():
    logging.basicConfig(level=logging.INFO)
    if not CHAT_ID:
        raise SystemExit("TELEGRAM_CHAT_ID is required")
    token = Path(TOKEN_FILE).read_text(encoding="utf-8").strip()
    if ":" not in token:
        raise SystemExit("Telegram bot token not configured")
    con = open_state()
    LOG.info("PureGamma RSS / TSY read-only heartbeat monitor started")
    while True:
        now = int(time.time())
        try:
            tick(con, now)
            flush_outbox(con, token, CHAT_ID, now)
        except sqlite3.Error:
            LOG.exception("state database unavailable; monitor cannot persist alerts")
        time.sleep(CYCLE_SECONDS)


if __name__ == "__main__":
    main()
