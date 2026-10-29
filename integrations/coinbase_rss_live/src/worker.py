"""Coinbase LIVE-only RSS strategy: autonomous event scanning, fail-closed execution.

No shadow or paper branches. Without an explicit live arm and approved limits
it monitors signals but never submits orders. No keys are printed to logs.
"""
import json,os,sqlite3,time
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from core import make_signal,make_intent_id,OFFICIAL_SOURCE_IDS
from shared import db,alert,status

RSS='/rss/events.db'
# Dedicated Coinbase-only secret file (COINBASE_* keys). Never mount the shared
# TSY .env here: it also holds Hyperliquid/Binance/Telegram secrets.
COINBASE_ENV='/run/coinbase-env'
ARM_KEYS={'PG_LIVE_TRADING','COINBASE_PORTFOLIO_ID','LIVE_ORDER_USD','LIVE_MAX_ORDER_USD','LIVE_MAX_DAILY_USD'}

def load_env(path):
    items={}
    for line in Path(path).read_text().splitlines():
        raw=line.strip()
        if not raw or raw.startswith('#') or '=' not in raw:continue
        k,v=raw.split('=',1)
        k=k.strip().removeprefix('export ')
        # Defense in depth: keep only Coinbase settings even if the file has more.
        if not k.startswith('COINBASE_'):continue
        items[k]=v.strip().strip('"').strip("'")
    return items


def load_runtime_env():
    env=load_env(COINBASE_ENV)
    # Arming and limits come only from the sidecar's own Compose environment.
    env.update({k:v for k,v in os.environ.items() if k in ARM_KEYS})
    return env


def inspect_events(now):
    con=sqlite3.connect(f'file:{RSS}?mode=ro',uri=True,timeout=8)
    con.row_factory=sqlite3.Row
    try:
        # events.source is a display label; authority comes only from the stable
        # article source ID (same approach as integrations/puregamma_rss/service.py).
        marks=','.join('?'*len(OFFICIAL_SOURCE_IDS))
        return [dict(row) for row in con.execute(f'''SELECT e.id,e.title,e.source,e.priority,e.trust,e.published,e.last_seen,
            (SELECT a.source FROM articles a WHERE a.event_id=e.id AND a.source IN ({marks}) LIMIT 1) AS source_id
            FROM events e WHERE e.last_seen>=? ORDER BY e.last_seen,e.id LIMIT 100''',(*sorted(OFFICIAL_SOURCE_IDS),now-180)).fetchall()]
    finally:con.close()


def is_armed(env):
    if env.get('PG_LIVE_TRADING')!='true':return False,'PG_LIVE_TRADING is not true'
    if not env.get('COINBASE_PORTFOLIO_ID'):return False,'dedicated Coinbase portfolio not configured'
    if not env.get('COINBASE_API_KEY_NAME') or not env.get('COINBASE_API_PRIVATE_KEY'):
        return False,'Coinbase API credentials not configured'
    try:
        q=Decimal(env.get('LIVE_ORDER_USD','0'))
        cap=Decimal(env.get('LIVE_MAX_ORDER_USD','0'))
        day=Decimal(env.get('LIVE_MAX_DAILY_USD','0'))
        if min(q,cap,day)<=0 or q>cap or q>day:return False,'invalid order/daily risk ceilings'
    except Exception:return False,'invalid order/daily risk ceilings'
    return True,'configured'


def handle_signal(con,env,signal,now):
    import coinbase_live
    quote=Decimal(env['LIVE_ORDER_USD']);daily=Decimal(env['LIVE_MAX_DAILY_USD'])
    # Absolute exposure cap for this sidecar's submitted orders (not whole-account).
    today=datetime.fromtimestamp(now,timezone.utc).strftime('%Y-%m-%d')
    row=con.execute("SELECT value FROM meta WHERE key=?",('daily_submitted:'+today,)).fetchone()
    submitted=Decimal(row[0]) if row else Decimal(0)
    if submitted+quote>daily:
        alert(con,'risk','⛔ Coinbase LIVE 风控拒绝：策略当日总下单额上限已达到。')
        return
    # `submitted` intents also block until fills/protection are reconciled by an
    # operator; automatic reconciliation of submitted orders is not implemented.
    existing=con.execute("SELECT count(*) FROM intents WHERE state IN ('submitted','unknown','preparing')").fetchone()[0]
    if existing:
        alert(con,'risk','⛔ Coinbase LIVE 风控阻断：存在未完成或需对账订单，拒绝增加新敞口。')
        return
    # One intent per event (event_id is the PRIMARY KEY). A `rejected` intent does
    # not block other events, but a priority/trust change gives the same event a
    # new inspected fingerprint; never re-trade it or crash on the INSERT.
    if con.execute("SELECT 1 FROM intents WHERE event_id=?",(signal.event_id,)).fetchone():
        alert(con,'risk','⛔ Coinbase LIVE 风控拒绝：该事件已有下单意图记录，不重复下单。')
        return
    broker=coinbase_live.LiveCoinbase(env)
    # Verify all read-side requirements before creating durable order intent.
    broker.preflight(signal.product)
    client_id=make_intent_id(signal.event_id)
    con.execute("INSERT INTO intents(event_id,client_id,state,created,note) VALUES (?,?,?,?,?)",
                (signal.event_id,client_id,'preparing',now,signal.rule))
    con.commit()
    try:
        result=broker.submit_market_with_protection(signal,quote,client_id,Decimal('0.006'),Decimal('0.012'))
    except coinbase_live.PreSubmitRejected as exc:
        # Raised only before create_order was attempted: no exchange order can
        # exist, so close the intent instead of freezing all future trading.
        con.execute("UPDATE intents SET state='rejected',note=? WHERE event_id=?",
                    (('pre-submit rejected: '+str(exc))[:300],signal.event_id))
        con.commit()
        alert(con,'risk','⛔ Coinbase LIVE 下单前检查拒绝（未提交任何委托）：'+str(exc)[:200])
        return
    except Exception as exc:
        # No second attempt: even a network timeout can mean an exchange order filled.
        con.execute("UPDATE intents SET state='unknown',note='LIVE outcome ambiguous; manual reconciliation required' WHERE event_id=?",(signal.event_id,))
        con.commit()
        alert(con,'critical','🚨 Coinbase LIVE 委托回执异常：状态 UNKNOWN，禁止任何新开仓，需按 client_order_id 人工核对。')
        return
    con.execute("UPDATE intents SET state='submitted',order_id=?,note='live submitted; fills not yet reconciled' WHERE event_id=?",
                (result['order_id'],signal.event_id))
    con.execute("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                ('daily_submitted:'+today,str(submitted+quote)))
    con.commit()
    alert(con,'trade',f"✅ Coinbase LIVE 已提交真实订单\n产品：{signal.product}\n数量上限：${quote}\nOrder ID：{result['order_id']}\n状态：已提交，成交与保护单仍需核对。")


def process_rows(conn,env,armed,rows,now):
    for item in rows:
        # NULL columns must not crash the loop (str(None) is fine for a key).
        fp=':'.join(str(item.get(k)) for k in ('id','priority','trust'))
        if conn.execute('SELECT 1 FROM inspected WHERE fingerprint=?',(fp,)).fetchone():continue
        conn.execute('INSERT OR IGNORE INTO inspected VALUES(?,?)',(fp,now))
        conn.commit()
        if int(item['last_seen'] or 0)<int(conn.execute("SELECT value FROM meta WHERE key='baseline'").fetchone()[0]):continue
        signal=make_signal(item,now=now)
        if signal:
            alert(conn,'signal',f'🔎 已识别可信 RSS 信号：{signal.rule}\n来源：{signal.source}\n标题：{signal.title[:200]}\n交易状态：'+('LIVE已启用' if armed else 'LIVE未激活'))
            if armed:handle_signal(conn,env,signal,now)


def run():
    Path('/state').mkdir(exist_ok=True)
    conn=db()
    env=load_runtime_env()
    ok,reason=is_armed(env)
    # Initial boot is only a baseline; no retroactive trading on old articles.
    if not conn.execute("SELECT value FROM meta WHERE key='baseline'").fetchone():
        conn.execute("INSERT INTO meta(key,value) VALUES('baseline',?)",(str(int(time.time())),))
        conn.commit()
    status(conn,'coinbase_live','armed' if ok else 'not_armed',reason)
    last_preflight=0
    while True:
        now=int(time.time())
        try:
            env=load_runtime_env()
            armed,reason=is_armed(env)
            process_rows(conn,env,armed,inspect_events(now),now)
            status(conn,'coinbase_live','armed' if armed else 'not_armed',reason)
        except Exception as exc:
            # Keep credentials / SDK exceptions entirely out of alert body/logs.
            row = conn.execute("SELECT value FROM meta WHERE key='last_fault_notice'").fetchone()
            if not row or now-int(row[0])>=1800:
                alert(conn,'critical','🚨 Coinbase LIVE 策略循环异常，需运维排查；本轮未确认下单成功。')
                conn.execute("INSERT INTO meta(key,value) VALUES('last_fault_notice',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",(str(now),))
                conn.commit()
            status(conn,'coinbase_live','fault','runtime failure: '+type(exc).__name__)
        time.sleep(15)

if __name__=='__main__':run()
