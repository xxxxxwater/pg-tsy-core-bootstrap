# PureGamma RSS V4 × PG-TSY × Coinbase Advanced **LIVE** (experimental integration)

## Verified host and runtime on 2026-10-23

- Target `instabox-pg-test` (4 vCPU, 8 GiB RAM, 20 GiB persistent /data volume).
- `/data/workspace/pg-tsy-core-bootstrap` exists. `pg-core /healthz` replied `mode=shadow`, `ready=true`, `feeds_connected=8/8`, `policy_decisions_total=0`.
- PureGamma RSS V4 `/status` replied `events=98`, `sent=2`, `pending=0`; `watcherguru_chat` previously 503.
- TSY's versioned `signal.v1` cannot represent venue COINBASE, and the present TSY daemon's live adapter is Hyperliquid/IBKR. **Do not set its `PG_RUN_MODE=live` expecting Coinbase orders.**
- Coinbase SDK read-only preflight exists in TSY; private `.env` is unreadable by the remote management agent (0600), and no key values were inspected.

## Architecture and explicitly scoped LIVE behavior

```text
Raw RSS/Atom → RSSHub → event normalization/dedup/classification (original titles)
       └─  /data/home/puregamma-event-collector/data/events.db  [READ ONLY]
                 ↓
  Coinbase Live RSS Signal Filter (strict A trust, SEC original title, <=120s)
                 ↓
   Durable signal / intent journal + rule version + event fingerprint
                 ↓
    Gates: explicit real Coinbase portfolio, spot BTC-USD, live flag, max order,
          max daily order notional, fresh product/orderbook, <=20bp spread
                 ↓
       Coinbase Advanced Trade GET key permissions, product, best bid/ask
                 ↓
      Preview market order + attached native bracket (stop 0.6% / TP 1.2%)
                 ↓
     CREATE ORDER **LIVE** with persistent deterministic client_order_id
                 ↓
      Coinbase response and order_id saved; status='submitted', then
      **HALT FURTHER ENTRIES** until order/fill/attached protection reconciled.
```

This is **not paper trading**. When armed it sends real Coinbase orders. It does not attempt to turn TSY's pre-existing shadow adapter live, and it must not be called a native PG-TSY Coinbase execution integration: that needs an explicit Rust venue registration and full OMS reconciliation.

**Critical limitations: do not arm until addressed**

1. This is a narrow experimental one-shot live RSS entry test, not an unattended production risk engine. `preparing`, `submitted` and `unknown` intents **block further orders indefinitely until manual reconciliation**. It does **not** automatically close positions. The attached stop/TP parameters are attempted through Coinbase's native order preview, but acceptance and protection must be verified against the actual portfolio/product. Stops are NOT guaranteed to fill, especially during price gaps or high volatility.
2. `BTC-USD` only, BUY only; event rule is *direct official SEC spot BTC ETF approval*, excludes proposals/rumors/denials. It will probably fire infrequently. It will not trade arbitrary WatcherGuru headlines.
3. No automatic order submission unless operator configures **all**: `PG_LIVE_TRADING=true`, `COINBASE_PORTFOLIO_ID`, `LIVE_ORDER_USD`, `LIVE_MAX_ORDER_USD`, `LIVE_MAX_DAILY_USD`. Defaults are all disabled / zero, and the program contains **no paper branch**.
4. Coinbase preview / trade permissions are required; errors fail closed. Failures before `create_order` (arming, caps, preflight, spread, preview, fees, read-side SDK errors) close the intent as `rejected` and do not block later signals; any failure during/after `create_order` (e.g. a network timeout after POST) freezes an `unknown` intent and never resubmits automatically. Only a dedicated Coinbase-only secret file (`COINBASE_ENV_FILE`) is mounted read-only; the shared TSY `.env` is never mounted.
5. No strategy performance claims; news latency, slippage, feed updating, market impact and bracket suitability have not been live-tested or measured.

## Local health checks and Telegram news

- The RSS V4 service owns Telegram news delivery, including the bilingual article and news digest. This integration does not send Telegram messages.
- The sidecar monitor still probes RSS `/status`, TSY `/healthz`, and its local strategy status every 60 seconds. Health transitions and faults appear in container logs.
- Existing and future sidecar alert rows stay in local `state/live.sqlite` as operational records; the monitor does not replay queued heartbeats, health alerts, trading signals, or strategy notices into the news chat.
- The Coinbase strategy's live arming and risk controls are unchanged. Inspect local status and logs for incidents.

## Installation on existing InstaBox by SSH

This package is designed to be uploaded into an isolated persistent folder under `/data/home/pg-rss-tsy-live` owned by the VM's `christse` user. DO NOT overwrite existing TSY/RSS projects.

On your Mac, after downloading and unzipping this package, run:

```bash
sh scripts/install_over_ssh.sh 'YOUR_INSTABOX_USER@ssh.instabox.sh'
```

The script checks the exact hostname and uploads only code. It creates an empty `secrets/coinbase.env` (0600) if missing and requires `COINBASE_ENV_FILE` in `.env`; it refuses to rebuild/restart (exit 3) when `.env` is armed or customized (`PG_LIVE_TRADING` not `false`). Existing deployments must add `COINBASE_ENV_FILE=./secrets/coinbase.env` and remove the obsolete `TSY_ENV_FILE`, `RSS_BOT_TOKEN_FILE` entries and `secrets/chat-id` (no longer mounted: the health monitor sends no Telegram messages). The sidecar starts **not armed** alongside a local health monitor. Before changing `PG_LIVE_TRADING` to true, validate your Coinbase dedicated portfolio's trade permissions, preview support, and set a real order budget and fee/stop policy.

Inspect:

```bash
ssh -p 2222 YOUR_INSTABOX_USER@ssh.instabox.sh \
  'cd "$HOME/pg-rss-tsy-live" && docker compose ps && docker compose logs --tail=50'
```

Do not paste Coinbase private keys or Telegram Bot Token into chat. Put **only** `COINBASE_API_KEY_NAME` and `COINBASE_API_PRIVATE_KEY` into `secrets/coinbase.env` (mode 0600) on the host; do not point `COINBASE_ENV_FILE` at the shared TSY `.env`, which also contains Hyperliquid, Binance and Telegram secrets. The worker ignores any non-`COINBASE_*` line in that file and reads arming/limits only from the sidecar's Compose environment.

## Explicit LIVE arming — only after trade key/portfolio preview is accepted

This is a LIVE environment with **no paper route**. Initial service installation is
intentionally disarmed because the user has not supplied a quote-dollar order
size, per-order cap, daily budget or confirmed isolated Coinbase Portfolio ID.
These are real-money constraints and must not be guessed. The operator can
activate in `~/pg-rss-tsy-live/.env` only after approving the limits. Example
structure (the values below are placeholders, NOT pre-approved parameters):

```dotenv
PG_LIVE_TRADING=true
COINBASE_PORTFOLIO_ID=<verified-dedicated-portfolio-uuid>
LIVE_ORDER_USD=<approved-usd-per-order>
LIVE_MAX_ORDER_USD=<approved-max-usd>
LIVE_MAX_DAILY_USD=<approved-daily-usd>
```

Then restart only the sidecar: `docker compose up -d --no-deps --force-recreate live-strategy`.
No modifications to the TSY main daemon. Confirm no transfers are allowed on
the API key; the sidecar checks `can_transfer=false`, `can_trade=true` and portfolio
UUID match. Coinbase native order preview and post-fill reconciliation still need
real-portfolio acceptance.

**IMPORTANT:** `get_api_key_permissions` only checks token scope, not full
operational readiness. The first accepted order must be supervised. No live
orders were placed during the construction/testing of this package.

## Hotfix 1.1 (2026-10-23)

- Explicit `not_armed` is a normal, safe state and **not** an incident.
- Monitor writes startup, minute probe results, queued heartbeat, and Telegram delivery result to logs without leaking secrets.
- Hourly operational heartbeats, three consecutive health-check failures before alerting and recovery notices.
- `scripts/audit_status.py` reads locally persisted Telegram delivery metadata without printing token values or Coinbase API material.
- Deployed on InstaBox `instabox-pg-test` and verified by the user's terminal evidence: `alert_id=8` delivered, `state=not_armed`, RSS healthy with 98 events and 2 formal messages, TSY 8/8 feeds at the time of verification. This is not evidence of Coinbase execution readiness.

Do not commit `.env`, `secrets/`, `state/`, sessions, database snapshots, or private API keys.

## Versioned prerelease package — `v0.2.1-rc.1`

Release: https://github.com/xxxxxwater/pg-tsy-core-bootstrap/releases/tag/v0.2.1-rc.1

OCI image: `ghcr.io/xxxxxwater/pg-tsy-coinbase-rss-sidecar:v0.2.1-rc.1`

```sh
docker pull ghcr.io/xxxxxwater/pg-tsy-coinbase-rss-sidecar:v0.2.1-rc.1
# Credential-free image identity check; does not start the trading service.
docker run --rm --network none --entrypoint id ghcr.io/xxxxxwater/pg-tsy-coinbase-rss-sidecar:v0.2.1-rc.1 -u
```

The supplied `compose.yaml` continues to **build from this checked-out source**; pulling the GHCR image alone does not replace, arm or restart any InstaBox containers. For an image-based rollout, explicitly review Compose, mounts, private credentials and checked-in version against the published digest before deploying. Both commands (`python /app/worker.py` and `python /app/monitor.py`) are baked into the same credential-free artifact, not an exchange-account initialization. Refer to [Release notes](../../docs/releases/v0.2.1-rc.1.md) for exclusions, checksums and the NO-GO boundary.
