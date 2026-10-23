import json, sqlite3, time, urllib.parse, urllib.request, urllib.error
from pathlib import Path

STATE='/state/live.sqlite'

def db():
    c=sqlite3.connect(STATE,timeout=15)
    c.row_factory=sqlite3.Row
    c.execute('PRAGMA busy_timeout=15000')
    c.execute('PRAGMA journal_mode=WAL')
    c.executescript('''
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY,value TEXT);
    CREATE TABLE IF NOT EXISTS inspected (fingerprint TEXT PRIMARY KEY,at INTEGER);
    CREATE TABLE IF NOT EXISTS intents (event_id TEXT PRIMARY KEY,client_id TEXT UNIQUE,
       state TEXT,created INTEGER,order_id TEXT,note TEXT);
    CREATE TABLE IF NOT EXISTS alerts (id INTEGER PRIMARY KEY AUTOINCREMENT,
       kind TEXT,text TEXT,created INTEGER,sent INTEGER,attempts INTEGER DEFAULT 0,next_at INTEGER DEFAULT 0);
    CREATE TABLE IF NOT EXISTS service_status (name TEXT PRIMARY KEY,ts INTEGER,state TEXT,detail TEXT);
    ''')
    c.commit()
    return c

def alert(con,kind,text):
    con.execute('INSERT INTO alerts(kind,text,created) VALUES (?,?,?)',(kind,text[:1300],int(time.time())))
    con.commit()

def status(con,name,state,detail):
    con.execute('INSERT INTO service_status VALUES(?,?,?,?) ON CONFLICT(name) DO UPDATE SET ts=excluded.ts,state=excluded.state,detail=excluded.detail',
                (name,int(time.time()),state,detail[:500]))
    con.commit()

def secret_file(path):
    return Path(path).read_text().strip()

def send_telegram(token,chat_id,body):
    payload=urllib.parse.urlencode({'chat_id':chat_id,'text':body[:3900],'disable_web_page_preview':'true'}).encode()
    req=urllib.request.Request('https://api.telegram.org/bot'+token+'/sendMessage',data=payload,method='POST')
    try:
        with urllib.request.urlopen(req,timeout=12) as resp:
            result=json.load(resp)
            return bool(result.get('ok')),0
    except urllib.error.HTTPError as e:
        delay=60
        try:delay=max(10,int(json.loads(e.read(4096)).get('parameters',{}).get('retry_after',60)))
        except (ValueError,TypeError,KeyError):pass
        return False,delay
    except Exception:
        return False,60
