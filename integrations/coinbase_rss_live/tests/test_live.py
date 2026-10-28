import sys,unittest
from pathlib import Path
from decimal import Decimal
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'src'))
from core import make_signal,risk_allow,make_intent_id
from worker import is_armed

class Tests(unittest.TestCase):
    def setUp(self):
        self.e={'id':'e1','title':'SEC approves spot Bitcoin ETF listing',
                'source':'SEC Press Releases','source_id':'sec_press','trust':'A','priority':'P1','published':2000}
    def test_explicit_approval(self):
        self.assertEqual(make_signal(self.e,now=2050).product,'BTC-USD')
    def test_no_trade_on_rumor(self):
        for text in ['SEC may approve spot Bitcoin ETF','SEC denies spot Bitcoin ETF approval','WatcherGuru SEC approves spot Bitcoin ETF']:
            item=dict(self.e,title=text)
            if text.startswith('Watcher'): item['trust']='B'
            self.assertIsNone(make_signal(item,now=2050),text)
    def test_stale_fails_closed(self):
        self.assertIsNone(make_signal(self.e,now=2300))
    def test_authority_required(self):
        for key,value in [('trust','C'),('source_id','watcherguru'),('source_id',None),('priority','P2')]:
            self.assertIsNone(make_signal(dict(self.e,**{key:value}),now=2050))
    def test_spoofed_source_name_rejected(self):
        # Display labels starting with "sec" must not confer authority.
        for name in ('SecurityWeek','SEC Press Releases','sec_press'):
            item=dict(self.e,source=name);item.pop('source_id')
            self.assertIsNone(make_signal(item,now=2050),name)
            self.assertIsNone(make_signal(dict(item,source_id='securityweek'),now=2050),name)
    def test_uncertainty_words_blocked(self):
        for text in ['SEC could approve spot Bitcoin ETF','SEC alleged to approve spot Bitcoin ETF']:
            self.assertIsNone(make_signal(dict(self.e,title=text),now=2050),text)
    def test_no_keyword_false_positive(self):
        self.assertIsNone(make_signal(dict(self.e,title='SEC proposes new bitcoin spot ETF rules'),now=2050))
    def test_spread_and_cap(self):
        self.assertTrue(risk_allow('10','15','100000','100010'))
        self.assertFalse(risk_allow('100','15','100000','100010'))
        self.assertFalse(risk_allow('10','15','100000','101000'))
        self.assertFalse(risk_allow('10','15','100000','99999'))
    def test_deterministic_order_id(self):
        self.assertEqual(make_intent_id('e1'),make_intent_id('e1'))
        self.assertNotEqual(make_intent_id('e1'),make_intent_id('e2'))
    def test_hard_arm(self):
        arm={'PG_LIVE_TRADING':'true','COINBASE_PORTFOLIO_ID':'portfolio-1','LIVE_ORDER_USD':'10',
             'LIVE_MAX_ORDER_USD':'10','LIVE_MAX_DAILY_USD':'10',
             'COINBASE_API_KEY_NAME':'k','COINBASE_API_PRIVATE_KEY':'p'}
        self.assertTrue(is_armed(arm)[0]);self.assertFalse(is_armed(dict(arm,PG_LIVE_TRADING='false'))[0]);
        self.assertFalse(is_armed(dict(arm,LIVE_ORDER_USD='100'))[0]);
        self.assertFalse(is_armed(dict(arm,COINBASE_PORTFOLIO_ID=''))[0]);
        self.assertFalse(is_armed(dict(arm,COINBASE_API_PRIVATE_KEY=''))[0]);
if __name__=='__main__':unittest.main()
