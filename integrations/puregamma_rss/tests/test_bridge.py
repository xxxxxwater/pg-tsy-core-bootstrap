import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from policy import candidate_from_event
from execution_boundary import (
    LiveReadiness, CoinbaseExecutionNotRegistered,
    dispatch_order, missing_live_gates,
)
import service


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.event = dict(
            id="evt-001", title="SEC approves spot Bitcoin ETF for listing",
            source_id="sec_press", priority="P1", trust="A",
            published=1000,
        )

    def test_strict_official_recent_signal(self):
        c = candidate_from_event(self.event, now=1050)
        self.assertEqual(c.product_id, "BTC-USD")
        self.assertEqual(c.disposition, "REVIEW_REQUIRED")
        self.assertFalse(hasattr(c, "side"))

    def test_rumors_stale_and_community_blocked(self):
        for title in (
            "SEC may approve spot Bitcoin ETF for listing",
            "SEC denies approval of spot Bitcoin ETF",
            "SEC proposes spot Bitcoin ETF for listing",
        ):
            self.assertIsNone(candidate_from_event({**self.event, "title": title}, now=1050))
        self.assertIsNone(candidate_from_event({**self.event, "trust": "B"}, now=1050))
        self.assertIsNone(candidate_from_event({**self.event, "source_id": "watcherguru"}, now=1050))
        self.assertIsNone(candidate_from_event(self.event, now=1300))

    def test_dispatch_is_impossible(self):
        with self.assertRaises(CoinbaseExecutionNotRegistered):
            dispatch_order({"symbol": "BTC-USD", "size": "100000"})
        self.assertEqual(len(missing_live_gates(LiveReadiness())), 9)


class PersistenceTests(unittest.TestCase):
    def test_first_read_baselines_and_new_candidates_only_once(self):
        with tempfile.TemporaryDirectory() as d:
            rss_db = str(Path(d) / "rss.sqlite")
            state_db = str(Path(d) / "state.sqlite")
            c = sqlite3.connect(rss_db)
            c.execute("CREATE TABLE events(id TEXT,title TEXT,source TEXT,priority TEXT,trust TEXT,published INTEGER,discovered INTEGER,last_seen INTEGER)")
            c.execute("CREATE TABLE articles(event_id TEXT, source TEXT)")
            c.execute("INSERT INTO events VALUES (?,?,?,?,?,?,?,?)",
                      ("old", "SEC approves spot Bitcoin ETF for listing", "SEC Press Releases",
                       "P1", "A", 1000, 1000, 1000))
            c.execute("INSERT INTO articles VALUES (?,?)", ("old","sec_press"))
            c.commit()
            c.close()
            state = service.open_state(state_db)
            self.assertEqual(service.new_feed_candidates(state, rss_db, 1010), 0)
            c = sqlite3.connect(rss_db)
            c.execute("INSERT INTO events VALUES (?,?,?,?,?,?,?,?)",
                      ("new", "SEC approves spot Bitcoin ETF for listing", "SEC Press Releases",
                       "P1", "A", 1020, 1020, 1020))
            c.execute("INSERT INTO articles VALUES (?,?)", ("new","sec_press"))
            c.commit()
            c.close()
            self.assertEqual(service.new_feed_candidates(state, rss_db, 1050), 1)
            self.assertEqual(service.new_feed_candidates(state, rss_db, 1051), 0)
            self.assertEqual(state.execute("SELECT COUNT(*) FROM outbox").fetchone()[0], 1)
            self.assertEqual(state.execute("SELECT COUNT(*) FROM candidates").fetchone()[0], 1)
            state.close()

    def test_health_faults_are_deduplicated(self):
        with tempfile.TemporaryDirectory() as d:
            c = service.open_state(str(Path(d) / "state.sqlite"))
            with patch.object(service, "new_feed_candidates", return_value=0):
                with patch.object(service, "check_health", return_value=(["RSS down"], [])):
                    # Realistic epoch seconds: the first tick is far past
                    # HEARTBEAT_SECONDS since the unset (0) last_heartbeat.
                    base = 1_700_000_000
                    for now in (base, base + 60, base + 120, base + 180):
                        service.tick(c, now)
            kinds = [row[0] for row in c.execute("SELECT kind FROM outbox").fetchall()]
            self.assertEqual(kinds.count("alert"), 1)
            self.assertEqual(kinds.count("heartbeat"), 1)
            c.close()

    def test_telegram_retries_without_duplicate_successful_record(self):
        with tempfile.TemporaryDirectory() as d:
            c = service.open_state(str(Path(d) / "state.sqlite"))
            service.enqueue(c, "heartbeat", "hello", 100)
            with patch.object(service, "send_message", return_value=(False, 70)):
                self.assertEqual(service.flush_outbox(c, "hidden", "id", 100), 0)
            row = c.execute("SELECT attempts,due_at,sent_at FROM outbox").fetchone()
            self.assertEqual((row["attempts"], row["due_at"], row["sent_at"]), (1,170,None))
            with patch.object(service, "send_message", return_value=(True, 0)):
                with patch.object(service.time, "sleep"):
                    self.assertEqual(service.flush_outbox(c, "hidden", "id", 171), 1)
            self.assertEqual(service.flush_outbox(c, "hidden", "id", 172), 0)
            c.close()

if __name__ == "__main__":
    unittest.main()
