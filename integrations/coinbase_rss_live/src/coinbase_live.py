"""Live Coinbase Advanced Trade adapter (never routes through TSY IBKR/Hyperliquid).

All write operations are guarded by a separate configured live arm + max notional,
by preview and permissions checks, and by durable intent recorded BEFORE POST.
An ambiguous POST freezes the intent and requires human reconciliation.
No automatic retry of ambiguous order submissions.
"""
import os
from decimal import Decimal
from core import risk_allow


class PreSubmitRejected(RuntimeError):
    """Raised only when no create_order request was attempted.

    Every failure before the create_order call (arming, caps, preflight,
    pricebook, spread, preview, fees, or read-side SDK/network errors) cannot
    have created an exchange order, so the intent may be closed as `rejected`.
    Anything raised during or after create_order is NOT this type and must be
    treated as ambiguous (`unknown`) by the caller.
    """


def as_dict(response):
    if hasattr(response,'to_dict'):
        return response.to_dict()
    if isinstance(response,dict):return response
    raise RuntimeError('unexpected Coinbase SDK response type')


class LiveCoinbase:
    def __init__(self,env):
        from coinbase.rest import RESTClient
        key=env.get('COINBASE_API_KEY_NAME')
        private=(env.get('COINBASE_API_PRIVATE_KEY') or '').replace('\\n','\n')
        if not key or not private:
            raise RuntimeError('Coinbase API credentials not configured in mounted restricted file')
        self.c=RESTClient(api_key=key, api_secret=private, timeout=15)
        self.env=env

    def preflight(self,product='BTC-USD'):
        p=as_dict(self.c.get_product(product))
        if p.get('product_id')!=product or p.get('trading_disabled') or p.get('cancel_only') or p.get('limit_only') or p.get('is_disabled'):
            raise PreSubmitRejected('product identity or trading availability check failed')
        perms=as_dict(self.c.get_api_key_permissions())
        if not perms.get('can_trade'):
            raise PreSubmitRejected('Coinbase key does not report can_trade=true')
        if not perms.get('can_view'):
            raise PreSubmitRejected('Coinbase key does not report can_view=true')
        if perms.get('can_transfer'):
            raise PreSubmitRejected('Coinbase LIVE requires a non-transfer trading key')
        if self.env.get('COINBASE_PORTFOLIO_ID') and perms.get('portfolio_uuid') != self.env['COINBASE_PORTFOLIO_ID']:
            raise PreSubmitRejected('trading key portfolio UUID does not match configured isolated portfolio')
        return p

    def submit_market_with_protection(self,signal,quote,client_id,stop_fraction,take_fraction):
        """Live market buy with a native attached bracket config (must pass preview).

        Caller guarantees persistence of intent before this method. The returned
        success is NOT a fill. Reconcile via order_id and fill history.
        """
        try:
            portfolio,quote,order,attachment,bid,ask=self._prepare_order(signal,quote,stop_fraction,take_fraction)
        except PreSubmitRejected:
            raise
        except Exception as exc:
            # Read-only/preview path only: no order can exist yet. Keep SDK
            # details (which may echo request data) out of the message.
            raise PreSubmitRejected('pre-submit check failed: '+type(exc).__name__) from exc
        # From here on any failure is ambiguous: the POST may have reached Coinbase.
        result=as_dict(self.c.create_order(client_id,signal.product,'BUY',order,
            retail_portfolio_id=portfolio,attached_order_configuration=attachment))
        if not result.get('success'):
            raise RuntimeError('Coinbase live submission not confirmed; reconcile intent')
        order_id=(result.get('success_response') or {}).get('order_id')
        if not order_id:raise RuntimeError('Coinbase response missing order_id; reconcile intent')
        return {'order_id':order_id,'client_id':client_id,'quote_usd':str(quote),'bid':str(bid),'ask':str(ask)}

    def _prepare_order(self,signal,quote,stop_fraction,take_fraction):
        """All checks that must pass before create_order; performs no writes."""
        if self.env.get('PG_LIVE_TRADING')!='true':
            raise PreSubmitRejected('live trading not armed')
        portfolio=self.env.get('COINBASE_PORTFOLIO_ID','')
        if not portfolio:
            raise PreSubmitRejected('dedicated Coinbase portfolio ID required')
        cap=Decimal(self.env.get('LIVE_MAX_ORDER_USD','0'))
        quote=Decimal(str(quote))
        if quote<=0 or cap<=0 or quote>cap:
            raise PreSubmitRejected('order above configured live notional ceiling')
        p=self.preflight(signal.product)
        # Bid/ask may be unavailable on a restricted API key: fail closed.
        book=as_dict(self.c.get_best_bid_ask(product_ids=[signal.product]))
        books=book.get('pricebooks') or []
        if not books:
            raise PreSubmitRejected('missing Coinbase best bid ask')
        top=books[0]
        if top.get('product_id')!=signal.product:raise PreSubmitRejected('pricebook product mismatch')
        bids,asks=top.get('bids') or [],top.get('asks') or []
        if not bids or not asks:raise PreSubmitRejected('empty bid ask')
        bid=Decimal(str(bids[0]['price']));ask=Decimal(str(asks[0]['price']))
        if not risk_allow(quote,cap,bid,ask):
            raise PreSubmitRejected('spread or order cap gate blocked')
        # Attached trigger bracket order protects exchange-side after eligible fills;
        # support depends on product/portfolio; reject if preview is not accepted.
        entry_price=ask
        tp=entry_price*(Decimal('1')+take_fraction)
        stop=entry_price*(Decimal('1')-stop_fraction)
        step=Decimal(str(p.get('price_increment') or '0.01'))
        tp=tp.quantize(step);stop=stop.quantize(step)
        attachment={'trigger_bracket_gtc':{'limit_price':str(tp),'stop_trigger_price':str(stop)}}
        order={'market_market_ioc':{'quote_size':str(quote)}}
        preview=as_dict(self.c.preview_order(signal.product,'BUY',order,
           retail_portfolio_id=portfolio,attached_order_configuration=attachment))
        if preview.get('errs') or preview.get('error_response') or preview.get('warning'):
            raise PreSubmitRejected('live order preview rejected or returned warning')
        # Fee cap must come from a confirmed preview, not an estimated fee tier.
        commission = preview.get('commission_total')
        if commission is None:
            raise PreSubmitRejected('Coinbase order preview lacks authoritative commission_total')
        if Decimal(str(commission)) < 0 or Decimal(str(commission)) > quote * Decimal('0.008'):
            raise PreSubmitRejected('Coinbase preview fee exceeds 80 bps')
        return portfolio,quote,order,attachment,bid,ask
