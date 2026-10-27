import sys,time,unittest,sqlite3
from pathlib import Path
from unittest.mock import patch,Mock
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'src'))
import monitor
from monitor import probe
class Check(unittest.TestCase):
    def setUp(self):
        self.c=sqlite3.connect(':memory:');self.c.row_factory=sqlite3.Row
        self.c.execute('CREATE TABLE service_status(name TEXT,ts INTEGER,state TEXT,detail TEXT)')
        self.now=1700000000
    def tearDown(self):self.c.close()
    def fake(self,url):
        if '8765' in url:return {'ok':True,'events':20,'sent':2,'pending':0,'sources':[{'id':'aggrnews','last_ok':self.now-30},{'id':'watcherguru_chat','last_ok':None,'seeded':0}]}
        return {'process_healthy':True,'ready':True,'mode':'shadow','feeds_connected':8,'feeds_total':8,'blocking_gates':[]}
    def test_disarmed_explicitly_reported(self):
        self.c.execute('INSERT INTO service_status VALUES (?,?,?,?)',('coinbase_live',self.now,'not_armed','no limits'))
        with patch('monitor.fetch',side_effect=self.fake):
            issues,info=probe(self.now,self.c)
        self.assertEqual(issues,[])
        self.assertTrue(any('not_armed' in i for i in info))
        self.assertTrue(any('TSY mode=shadow' in i for i in info))
    def test_healthy_armed(self):
        self.c.execute('INSERT INTO service_status VALUES (?,?,?,?)',('coinbase_live',self.now,'armed','configured'))
        with patch('monitor.fetch',side_effect=self.fake):
            issues,info=probe(self.now,self.c)
        self.assertEqual(issues,[])
    def test_dead_strategy_detected(self):
        self.c.execute('INSERT INTO service_status VALUES (?,?,?,?)',('coinbase_live',self.now-200,'armed','configured'))
        with patch('monitor.fetch',side_effect=self.fake):
            issues,info=probe(self.now,self.c)
        self.assertIn('Coinbase LIVE strategy loop no heartbeat',issues)
    def test_monitor_keeps_probing_without_telegram_delivery(self):
        sender=Mock();enqueue=Mock()
        with patch('monitor.db',return_value=self.c), \
             patch('monitor.probe',return_value=(['TSY not ready'],[])) as health, \
             patch('monitor.time.sleep',side_effect=[None,None,StopIteration]), \
             patch('monitor.time.time',return_value=self.now), \
             patch('shared.send_telegram',sender), \
             patch('shared.alert',enqueue):
            with self.assertRaises(StopIteration):
                monitor.main()
        self.assertEqual(health.call_count,3)
        sender.assert_not_called()
        enqueue.assert_not_called()

if __name__=='__main__':unittest.main()

