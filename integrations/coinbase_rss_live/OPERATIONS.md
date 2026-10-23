# InstaBox operations: RSS V4 + PG-TSY + Coinbase live sidecar

## Deployment contract

- Host: `instabox-pg-test`, persistent runtime: `/data/home/pg-rss-tsy-live`.
- Source code: `integrations/coinbase_rss_live/` in `xxxxxwater/pg-tsy-core-bootstrap` on `main`.
- Existing TSY/RSS programs are separately deployed and **not restarted** by the monitor-only hotfix.
- Docker Compose service names `live-strategy`, `health-monitor`.
- Coinbase strategy remains **not armed** until separately approved sizing, portfolio and verified order-risk controls. An active Docker container does not indicate authorization.
- Default TSY engine remains `PG_RUN_MODE=shadow`; switching its `live` mode does not create a Coinbase venue.

## Read-only checks (run inside InstaBox or via SSH)

```sh
cd "$HOME/pg-rss-tsy-live"
docker compose ps
docker compose logs --tail=50 health-monitor
python3 scripts/audit_status.py
curl -fsS http://127.0.0.1:8765/status
curl -fsS http://127.0.0.1:8080/healthz
```

For a fresh checkout, do not use `scripts/apply_monitor_hotfix_over_ssh.sh` as an initial installer: it **only** updates an existing `pg-rss-tsy-live` directory. The initial installer requires a privately configured `secrets/chat-id` file. Use a literal, quoted `user@ssh.instabox.sh` as its first parameter to avoid hidden Unicode characters pasted into the interactive SSH login prompt.

## Verified deployment log: October 23, 2026

From the operator's terminal following Hotfix 1.1:

- `health-monitor`: rebuilt and restarted without restarting TSY, RSSHub, LibreTranslate or the strategy sidecar.
- `probe result=HEALTHY issues=0 consecutive_failures=0`.
- `periodic heartbeat enqueued`; `Telegram notification sent; alert_id=8`.
- Audit: `alert_id=8 kind=heartbeat telegram_sent=yes`, plus earlier historical delivery rows.
- Strategy status: `service=coinbase_live state=not_armed`; no order intents recorded.
- RSS snapshot: `ok=True events=98 sent=2 pending=0`.
- PG-TSY snapshot: `mode=shadow ready=True feeds=8/8`.

These are **point-in-time observations** only. They do not prove 24/7 uptime, Coinbase order execution, fills, or safety of real-money automation. Later health may differ: the market-feed count briefly fell to 6/8 and recovered in subsequent probes.

## Monitor logic and alerts

- Polls RSS + TSY health plus `service_status` in the local SQLite state every 60 seconds.
- `coinbase_live=not_armed` is expected and should appear in heartbeats, not outage alerts.
- After three consecutive failing probes: enqueue and deliver a Telegram fault alert.
- After recovery: enqueue a recovery notice; periodic status every hour.
- State and delivery metadata remain in local `state/live.sqlite`; do not commit or copy it to GitHub.
- Same-host watchdog cannot detect complete InstaBox auto-suspend or host outage. Configure an independent external watcher when continuous alerts are required.

## Security, rollback and change review

Never commit `.env`, Coinbase key material, Telegram tokens, chat recipient IDs, Docker volumes, sessions, `/state`, backups or screenshots of credentials. Only the pre-existing private TSY environment is mounted read-only in the strategy container; the monitoring container receives only the RSS Bot token and the chat ID. The `README.md` in this integration explains the intentionally restricted LIVE gates.

To roll back **monitor-only** edits after a failed upgrade: restore the timestamped `src/monitor.py.backup.*` captured by the operator, rebuild `health-monitor` only, and recheck `scripts/audit_status.py`. Do not change `PG_LIVE_TRADING` during a monitoring rollback.

The execution sidecar is experimental and lacks fully automated fill/fees/protection-order reconciliation. Treat the LIVE path as NO-GO for unattended production use until native OMS ownership and reconciliation acceptance is complete.
