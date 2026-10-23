"""Coinbase LIVE-only RSS strategy: autonomous event scanning, fail-closed execution.

No shadow or paper branches. Without an explicit live arm and approved limits
it monitors signals but never submits orders. No keys are printed to logs.
"""
import json,os,sqlite3,time
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from core import make_signal,make_intent_id
from shared import db,alert,status

RSS='/rss/events.db'

def load_env(path):
    items={}
    for line in Path(path).read_text().splitlines():
        raw=line.strip()
        if not raw or raw.startswith('#') or '=' not in raw:continue
        k,v=raw.split('=',1)
        k=k.strip().removeprefix('export ')
        # Support the format used by the existing TSY environment file.
        items[k]=v.strip().strip('"').strip("'")
    return items


def inspect_events(now):
    con=sqlite3.connect(f'file:{RSS}?mode=ro',uri=True,timeout=8)
    con.row_factory=sqlite3.Row
    try:
        return [dict(row) for row in con.execute('''SELECT id,title,source,priority,trust,published,last_seen
            FROM events WHERE last_seen>=? ORDER BY last_seen,id LIMIT 100''',(now-180,)).fetchall()]
    finally:con.close()


def is_armed(env):
    if env.get('PG_LIVE_TRADING')!='true':return False,'PG_LIVE_TRADING is not true'
    if not env.get('COINBASE_PORTFOLIO_ID'):return False,'dedicated Coinbase portfolio not configured'
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
    existing=con.execute("SELECT count(*) FROM intents WHERE state IN ('submitted','unknown','preparing')").fetchone()[0]
    if existing:
        alert(con,'risk','⛔ Coinbase LIVE 风控阻断：存在未完成或需对账订单，拒绝增加新敞口。')
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


def run():
    Path('/state').mkdir(exist_ok=True)
    conn=db()
    env=load_env('/run/tsy-env')
    env.update({k:v for k,v in os.environ.items() if k in {'PG_LIVE_TRADING','COINBASE_PORTFOLIO_ID','LIVE_ORDER_USD','LIVE_MAX_ORDER_USD','LIVE_MAX_DAILY_USD'}})
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
            env=load_env('/run/tsy-env')
            env.update({k:v for k,v in os.environ.items() if k in {'PG_LIVE_TRADING','COINBASE_PORTFOLIO_ID','LIVE_ORDER_USD','LIVE_MAX_ORDER_USD','LIVE_MAX_DAILY_USD'}})
            armed,reason=is_armed(env)
            rows=inspect_events(now)
            for item in rows:
                fp=item['id']+':'+item['priority']+':'+item['trust']
                if conn.execute('SELECT 1 FROM inspected WHERE fingerprint=?',(fp,)).fetchone():continue
                conn.execute('INSERT OR IGNORE INTO inspected VALUES(?,?)',(fp,now))
                conn.commit()
                if item['last_seen']<int(conn.execute("SELECT value FROM meta WHERE key='baseline'").fetchone()[0]):continue
                signal=make_signal(item,now=now)
                if signal:
                    alert(conn,'signal',f'🔎 已识别可信 RSS 信号：{signal.rule}\n来源：{signal.source}\n标题：{signal.title[:200]}\n交易状态：'+('LIVE已启用' if armed else 'LIVE未激活'))
                    if armed:handle_signal(conn,env,signal,now)
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
