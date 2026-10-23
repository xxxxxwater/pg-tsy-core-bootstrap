#!/usr/bin/env python3
"""Read-only status of isolated RSS/TSY heartbeat; never prints credentials or messages."""
import datetime as dt
import json
from pathlib import Path
import sqlite3
import time
import urllib.request

base=Path(__file__).resolve().parents[1]
path=base/'state/live.sqlite'
if not path.is_file():
    raise SystemExit('live.sqlite not present (container state not initialized)')
now=int(time.time())
with sqlite3.connect('file:'+str(path)+'?mode=ro',uri=True) as con:
    con.row_factory=sqlite3.Row
    print('=== Strategy service status ===')
    for r in con.execute('SELECT name,state,ts,detail FROM service_status ORDER BY name'):
        age=now-int(r['ts'])
        print('service=%s state=%s age_seconds=%d detail=%s' % (r['name'],r['state'],age,str(r['detail'])[:160]))
    print('=== Telegram notifications (recent, metadata only) ===')
    for r in con.execute('SELECT id,kind,created,sent,attempts,next_at FROM alerts ORDER BY id DESC LIMIT 12'):
        sent='yes' if r['sent'] else 'no'
        print('id=%s kind=%s telegram_sent=%s attempts=%s age_seconds=%d next_due_seconds=%d' % (
              r['id'],r['kind'],sent,r['attempts'],now-r['created'],max(0,int(r['next_at'])-now)))
    print('=== Coinbase order intentions ===')
    for r in con.execute('SELECT state,COUNT(*) AS n FROM intents GROUP BY state'):
        print('intent_state=%s count=%d'%(r['state'],r['n']))
print('=== Live endpoint probes ===')
for name,url in [('rss','http://127.0.0.1:8765/status'),('tsy','http://127.0.0.1:8080/healthz')]:
    try:
        with urllib.request.urlopen(url,timeout=4) as response:
            data=json.load(response)
        if name=='rss':
            print('rss ok=%s events=%s sent=%s pending=%s' % tuple(data.get(k) for k in ('ok','events','sent','pending')))
        else:
            print('tsy mode=%s ready=%s feeds=%s/%s' % tuple(data.get(k) for k in ('mode','ready','feeds_connected','feeds_total')))
    except Exception as exc:
        print(name+' probe_error='+type(exc).__name__)
