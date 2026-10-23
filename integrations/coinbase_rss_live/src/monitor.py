"""Independent health/heartbeat daemon for RSS V4 and TSY + Coinbase live sidecar."""
import json,sqlite3,time,urllib.request,urllib.error,logging
from shared import db,alert,status,secret_file,send_telegram
from pathlib import Path

TOKEN='/run/telegram-token'
CHAT_ID_FILE='/run/chat-id'
LOG = logging.getLogger('health-monitor')


def fetch(url):
    with urllib.request.urlopen(url,timeout=6) as response:
        if response.status!=200:raise RuntimeError('non-200 health')
        return json.load(response)


def probe(now,conn):
    problems=[];info=[]
    try:
        rss=fetch('http://127.0.0.1:8765/status')
        if not rss.get('ok'):problems.append('RSS status not OK')
        info.append('RSS 事件=%d 已发=%d 排队=%d' % (rss.get('events',0),rss.get('sent',0),rss.get('pending',0)))
        for source in rss.get('sources',[]):
            sid=source.get('id','')
            if sid=='watcherguru_chat' and not source.get('seeded'):continue
            age=now-int(source.get('last_ok') or 0)
            threshold=4200 if sid=='rsshub_releases' else 1200
            if age>threshold:problems.append('RSS source stale: '+sid)
        if rss.get('pending',0)>40:problems.append('RSS Telegram queue backlog')
    except Exception:problems.append('RSS V4 endpoint unreachable')
    try:
        tsy=fetch('http://127.0.0.1:8080/healthz')
        if not tsy.get('process_healthy') or not tsy.get('ready'):problems.append('TSY not ready')
        feeds,total=tsy.get('feeds_connected',0),tsy.get('feeds_total',0)
        if not total or feeds<total:problems.append('TSY feed drop %s/%s'%(feeds,total))
        info.append('TSY mode=%s feeds=%s/%s gates=%s'%(tsy.get('mode','unknown'),feeds,total,len(tsy.get('blocking_gates') or [])))
    except Exception:problems.append('TSY /healthz unreachable')
    row=conn.execute("SELECT ts,state,detail FROM service_status WHERE name='coinbase_live'").fetchone()
    if row is None or now-int(row['ts'])>120:
        problems.append('Coinbase LIVE strategy loop no heartbeat')
    else:
        info.append('Coinbase LIVE=%s (%s)'%(row['state'],row['detail']))
        if row['state']=='fault':problems.append('Coinbase LIVE strategy fault')
        # An unarmed adapter is intentional and safe; never page as a system outage.
        if row['state']=='not_armed':info.append('Coinbase LIVE 实盘未授权（安全锁定）')
    return problems,info


def main():
    logging.Formatter.converter = time.gmtime
    logging.basicConfig(level=logging.INFO,format='%(asctime)sZ %(levelname)s %(name)s %(message)s')
    LOG.info('monitor process starting; probe interval=60s; regular Telegram heartbeat interval=3600s')
    conn=db();token=secret_file(TOKEN);chat_id=secret_file(CHAT_ID_FILE)
    last_beat=0;last_problems=None;failure_count=0
    while True:
        now=int(time.time())
        problems,info=probe(now,conn)
        failure_count=failure_count+1 if problems else 0
        state='DOWN' if failure_count>=3 else 'HEALTHY'
        # Check every minute, alert after 3 consecutive failures, recover immediately.
        new_problems=tuple(sorted(problems)) if state=='DOWN' else tuple()
        LOG.info('probe result=%s issues=%d consecutive_failures=%d',state,len(problems),failure_count)
        if last_problems!=new_problems:
            if last_problems is not None:
                if new_problems:
                    alert(conn,'health','🚨 系统健康告警\n'+'\n'.join('• '+p for p in new_problems))
                    LOG.warning('health state transition to DOWN, alert queued; issue_count=%d',len(new_problems))
                elif last_problems:
                    alert(conn,'health','✅ 系统健康恢复\n'+'\n'.join(info))
                    LOG.info('health state recovered; Telegram recovery alert queued')
            last_problems=new_problems
        if now-last_beat>=3600:
            alert(conn,'heartbeat','💓 RSS V4 + TSY + Coinbase LIVE 心跳\n'+'\n'.join(info)+('\n问题：'+', '.join(problems) if problems else '\n检查结果：正常'))
            last_beat=now
            LOG.info('periodic heartbeat enqueued')
        for item in conn.execute("SELECT id,text,attempts FROM alerts WHERE sent IS NULL AND next_at<=? ORDER BY id LIMIT 10",(now,)).fetchall():
            ok,delay=send_telegram(token,chat_id,item['text'])
            if ok:
                conn.execute('UPDATE alerts SET sent=? WHERE id=?',(now,item['id']))
                conn.commit()
                LOG.info('Telegram notification sent; alert_id=%d',item['id'])
            else:
                conn.execute('UPDATE alerts SET attempts=attempts+1,next_at=? WHERE id=?',(now+max(delay,60),item['id']))
                conn.commit()
                LOG.warning('Telegram delivery failed; alert_id=%d retry_in=%ds',item['id'],max(delay,60))
                break
            time.sleep(1.1)
        time.sleep(60)

if __name__=='__main__':main()
