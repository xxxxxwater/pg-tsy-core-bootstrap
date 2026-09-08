# Telegram operator control plane

Telegram is a relay and dashboard, not a privileged shell.

## Menu

| Command | Meaning |
| --- | --- |
| `/start` | Request start/resume after startup/reconcile/risk gates pass |
| `/performance` | PnL/performance summary |
| `/status` | Bot, venue, strategy, reconcile and risk status |
| `/logs [n]` | Bounded tail of recent structured logs |
| `/emergency_exit` | Request reduce-only flatten of strategy-owned exposure, then halt |
| `/scripts` | List allowlisted strategy definitions |
| `/reload_script <name>` | Reload an allowlisted strategy after validation |
| `/latency` | Websocket/TWS receive and order-path latency metrics |

## Security

- allowlist Telegram user IDs and chat IDs;
- token comes only from Secrets Manager/environment;
- deny unknown users without revealing account state;
- rate-limit commands;
- journal every mutating command with actor/time/request id;
- `/reload_script` accepts a logical allowlisted name, never a path or shell string;
- `/emergency_exit` is designed to be idempotent;
- Telegram cannot toggle `PG_LIVE_TRADING` by itself.

## Architecture

```text
Telegram
   |
 teloxide transport
   |
 auth + parser
   |
 ControlCommand
   |
 journal/audit
   |
 core command bus
   +--> query status/performance/logs/latency
   +--> start/reload request -> startup/risk gates
   +--> emergency exit -> reduce-only planner -> OMS -> execution -> halt
```
