import sys,unittest
from pathlib import Path
from unittest.mock import Mock
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'src'))
from coinbase_live import LiveCoinbase
from core import Signal

class SDK:
    def get_product(self,symbol):
        return {'product_id':symbol,'trading_disabled':False,'price_increment':'0.01'}
    def get_api_key_permissions(self):
        return {'can_trade':True,'can_view':True,'can_transfer':False,'portfolio_uuid':'dedicated'}
    def get_best_bid_ask(self,**kw):
        return {'pricebooks':[{'product_id':'BTC-USD','bids':[{'price':'80000'}],'asks':[{'price':'80002'}]}]}
    def preview_order(self,*args,**kw):
        return {'commission_total':'0.04','errs':[],'warning':[]}
    def create_order(self,*args,**kw):
        return {'success':True,'success_response':{'order_id':'cb-live-order-123'}}
class AdapterTests(unittest.TestCase):
    def setUp(self):
        self.adapter=LiveCoinbase.__new__(LiveCoinbase)
        self.adapter.c=SDK()
        self.adapter.env={'PG_LIVE_TRADING':'true','COINBASE_PORTFOLIO_ID':'dedicated','LIVE_MAX_ORDER_USD':'10'}
        self.signal=Signal('ev','BTC-USD','BUY','sec_rule',0.8,'test','SEC',1000)
    def test_live_requires_hard_arm(self):
        self.adapter.env['PG_LIVE_TRADING']='false'
        with self.assertRaisesRegex(RuntimeError,'not armed'):
            self.adapter.submit_market_with_protection(self.signal,'10','some-client-id',__import__('decimal').Decimal('0.006'),__import__('decimal').Decimal('0.012'))
    def test_correct_portfolio_and_nontransfer_permissions(self):
        self.adapter.preflight('BTC-USD')
        self.adapter.env['COINBASE_PORTFOLIO_ID']='different'
        with self.assertRaisesRegex(RuntimeError,'portfolio UUID'):
            self.adapter.preflight('BTC-USD')
    def test_live_order_preview_then_submission(self):
        from decimal import Decimal
        res=self.adapter.submit_market_with_protection(self.signal,'10','cid',Decimal('0.006'),Decimal('0.012'))
        self.assertEqual(res['order_id'],'cb-live-order-123')
    def test_preview_fee_rejected(self):
        self.adapter.c.preview_order=lambda *a,**k:{'commission_total':'1.00','errs':[],'warning':[]}
        from decimal import Decimal
        with self.assertRaisesRegex(RuntimeError,'fee exceeds'):
            self.adapter.submit_market_with_protection(self.signal,'10','cid',Decimal('0.006'),Decimal('0.012'))
    def test_no_orders_on_transfers_enabled(self):
        self.adapter.c.get_api_key_permissions=lambda:{'can_trade':True,'can_view':True,'can_transfer':True,'portfolio_uuid':'dedicated'}
        with self.assertRaisesRegex(RuntimeError,'non-transfer'):
            self.adapter.preflight('BTC-USD')
if __name__=='__main__':unittest.main()
