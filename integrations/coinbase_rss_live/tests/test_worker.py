import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'src'))
import coinbase_live
import shared
import worker
from coinbase_live import PreSubmitRejected
from core import Signal

ENV={'PG_LIVE_TRADING':'true','COINBASE_PORTFOLIO_ID':'p','LIVE_ORDER_USD':'10',
     'LIVE_MAX_ORDER_USD':'10','LIVE_MAX_DAILY_USD':'50'}


def fake_broker(exc=None):
    class Broker:
        def __init__(self,env):pass
        def preflight(self,product):return {}
        def submit_market_with_protection(self,*a):
            if exc:raise exc
            return {'order_id':'o1'}
    return Broker


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory()
        with patch.object(shared,'STATE',str(Path(self.tmp.name)/'live.sqlite')):
            self.con=shared.db()
        self.signal=Signal('ev1','BTC-USD','BUY','rule',0.8,'t','SEC',1000)
    def tearDown(self):
        self.con.close();self.tmp.cleanup()
    def state(self,event='ev1'):
        return self.con.execute('SELECT state FROM intents WHERE event_id=?',(event,)).fetchone()[0]


class SubmitOutcomeTests(Base):
    def run_with(self,exc):
        with patch.object(coinbase_live,'LiveCoinbase',fake_broker(exc)):
            worker.handle_signal(self.con,ENV,self.signal,1000)

    def test_pre_submit_rejection_closes_intent_and_does_not_block(self):
        self.run_with(PreSubmitRejected('spread or order cap gate blocked'))
        self.assertEqual(self.state(),'rejected')
        self.assertIsNone(self.con.execute("SELECT value FROM meta WHERE key LIKE 'daily_submitted:%'").fetchone())
        # A later signal is not blocked by the rejected intent.
        self.signal=Signal('ev2','BTC-USD','BUY','rule',0.8,'t','SEC',1000)
        self.run_with(None)
        self.assertEqual(self.state('ev2'),'submitted')

    def test_ambiguous_failure_is_unknown_and_blocks(self):
        for exc in (TimeoutError('read timeout'),RuntimeError('Coinbase response missing order_id; reconcile intent')):
            with self.subTest(exc=exc):
                self.con.execute('DELETE FROM intents');self.con.commit()
                self.run_with(exc)
                self.assertEqual(self.state(),'unknown')
        self.signal=Signal('ev2','BTC-USD','BUY','rule',0.8,'t','SEC',1000)
        self.run_with(None)
        self.assertIsNone(self.con.execute("SELECT 1 FROM intents WHERE event_id='ev2'").fetchone())

    def test_resurfaced_event_with_rejected_intent_is_not_retraded(self):
        # A priority/trust change gives the same event a new inspected
        # fingerprint; its earlier `rejected` intent does not block globally.
        self.run_with(PreSubmitRejected('spread or order cap gate blocked'))
        self.assertEqual(self.state(),'rejected')
        calls=[]
        class Broker:
            def __init__(self,env):calls.append('init')
            def preflight(self,product):calls.append('preflight')
            def submit_market_with_protection(self,*a):
                calls.append('submit');return {'order_id':'o2'}
        with patch.object(coinbase_live,'LiveCoinbase',Broker):
            worker.handle_signal(self.con,ENV,self.signal,1000)
        self.assertEqual(calls,[])
        self.assertEqual(self.state(),'rejected')
        self.assertEqual(self.con.execute('SELECT count(*) FROM intents').fetchone()[0],1)


class AdapterBoundaryTests(unittest.TestCase):
    """Only failures before create_order are PreSubmitRejected."""
    def setUp(self):
        sys.path.insert(0,str(Path(__file__).resolve().parent))
        from test_coinbase_adapter import SDK
        self.calls=[]
        sdk=SDK();orig=sdk.create_order
        def create(*a,**k):
            self.calls.append(a);return orig(*a,**k)
        sdk.create_order=create
        self.sdk=sdk
        self.a=coinbase_live.LiveCoinbase.__new__(coinbase_live.LiveCoinbase)
        self.a.c=sdk;self.a.env={'PG_LIVE_TRADING':'true','COINBASE_PORTFOLIO_ID':'dedicated','LIVE_MAX_ORDER_USD':'10'}
        self.sig=Signal('ev','BTC-USD','BUY','r',0.8,'t','SEC',1000)
    def submit(self):
        return self.a.submit_market_with_protection(self.sig,'10','cid',Decimal('0.006'),Decimal('0.012'))
    def test_read_side_sdk_error_is_pre_submit(self):
        def boom(**k):raise ConnectionError('network')
        self.sdk.get_best_bid_ask=boom
        with self.assertRaises(PreSubmitRejected):self.submit()
        self.assertEqual(self.calls,[])
    def test_not_armed_and_cap_are_pre_submit(self):
        self.a.env['PG_LIVE_TRADING']='false'
        with self.assertRaises(PreSubmitRejected):self.submit()
        self.a.env['PG_LIVE_TRADING']='true';self.a.env['LIVE_MAX_ORDER_USD']='5'
        with self.assertRaises(PreSubmitRejected):self.submit()
    def test_create_order_failures_are_not_pre_submit(self):
        def boom(*a,**k):raise TimeoutError('timeout')
        self.sdk.create_order=boom
        with self.assertRaises(TimeoutError):self.submit()
        self.sdk.create_order=lambda *a,**k:{'success':False}
        with self.assertRaises(RuntimeError) as ctx:self.submit()
        self.assertNotIsInstance(ctx.exception,PreSubmitRejected)


class ProcessRowsTests(Base):
    def test_null_columns_are_inspected_without_crash(self):
        self.con.execute("INSERT INTO meta VALUES('baseline','0')");self.con.commit()
        rows=[{'id':'e1','title':None,'source':None,'source_id':None,'priority':None,
               'trust':None,'published':None,'last_seen':1000}]
        worker.process_rows(self.con,{},False,rows,1000)
        self.assertEqual(self.con.execute('SELECT fingerprint FROM inspected').fetchall()[0][0],'e1:None:None')

class EnvFileTests(unittest.TestCase):
    def test_only_coinbase_keys_are_loaded(self):
        with tempfile.TemporaryDirectory() as d:
            f=Path(d)/'coinbase.env'
            f.write_text('COINBASE_API_KEY_NAME="org/key"\nexport COINBASE_API_PRIVATE_KEY=abc\n'
                         'HYPERLIQUID_PRIVATE_KEY=0xdead\nBINANCE_API_SECRET=s\nTELEGRAM_BOT_TOKEN=t\n'
                         'PG_LIVE_TRADING=true\n# comment\n')
            env=worker.load_env(str(f))
        self.assertEqual(env,{'COINBASE_API_KEY_NAME':'org/key','COINBASE_API_PRIVATE_KEY':'abc'})

    def test_arming_only_from_process_environment(self):
        with tempfile.TemporaryDirectory() as d:
            f=Path(d)/'coinbase.env';f.write_text('COINBASE_PORTFOLIO_ID=file\n')
            with patch.object(worker,'COINBASE_ENV',str(f)), \
                 patch.dict(worker.os.environ,{'PG_LIVE_TRADING':'false','COINBASE_PORTFOLIO_ID':'compose'},clear=True):
                env=worker.load_runtime_env()
        self.assertEqual(env,{'PG_LIVE_TRADING':'false','COINBASE_PORTFOLIO_ID':'compose'})


class InspectEventsTests(unittest.TestCase):
    def test_source_id_comes_from_official_article_only(self):
        import sqlite3
        with tempfile.TemporaryDirectory() as d:
            path=str(Path(d)/'events.db');c=sqlite3.connect(path)
            c.execute('CREATE TABLE events(id TEXT,title TEXT,source TEXT,priority TEXT,trust TEXT,published INTEGER,last_seen INTEGER)')
            c.execute('CREATE TABLE articles(event_id TEXT,source TEXT)')
            c.executemany('INSERT INTO events VALUES(?,?,?,?,?,?,?)',[
                ('a','SEC approves spot Bitcoin ETF','SEC Press Releases','P1','A',1000,1000),
                ('b','SEC approves spot Bitcoin ETF','SecurityWeek','P1','A',1000,1000)])
            c.executemany('INSERT INTO articles VALUES(?,?)',[('a','sec_press'),('b','securityweek')])
            c.commit();c.close()
            with patch.object(worker,'RSS',path):
                rows={r['id']:r for r in worker.inspect_events(1000)}
        self.assertEqual(rows['a']['source_id'],'sec_press')
        self.assertIsNone(rows['b']['source_id'])
        self.assertIsNotNone(worker.make_signal(rows['a'],now=1050))
        self.assertIsNone(worker.make_signal(rows['b'],now=1050))

if __name__=='__main__':unittest.main()
