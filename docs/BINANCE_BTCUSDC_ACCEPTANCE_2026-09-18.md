# Binance BTCUSDC PM — evidence-based acceptance record (2026-09-18)

**Release decision: BLOCKED — not a complete live trading loop or unattended production system.**
This record is a point-in-time engineering audit. Do not promote it into proof of profitable execution or production readiness. Leave live order submission disabled, do not register the new Binance adapter with the production daemon, and do not modify the incumbent Freqtrade/Docker PM bot or its manually owned positions.

## Verified automated checks

- GitHub Actions CI run `35342987631`, commit `6fa6107e753f7d357449f43846ca06f4b9cdbf53`: all seven jobs passed, including `cargo fmt --check`, full-workspace Clippy/test, `cargo check -p pg-control --features telegram`, Binance contract tests, JEV contract tests, Python, lockfile and Compose. https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35342987631
- The Telegram borrow-conflict repair (`f488b2cefa9b1958e17a6c27bd6b222d476240c7`) scopes the regular-rate queue borrow before accessing refresh/emergency cooldown maps; a test checks that cooldown rejection does not consume normal allowance. This was checked with the actual Telegram feature, not only default workspace features.
- Separate read-only, ignored Rust public transport test lives in `rust/adapters/pg-binance/tests/live_public.rs`. The `binance-public-live` workflow is explicitly operator-invoked only; it has **no secrets** and `PG_LIVE_TRADING=false`. It intentionally fails instead of accepting an unavailable depth snapshot or faking one.

## Actual public exchange observations and their limitations

1. GitHub-hosted Rust probe run `35343735329`: opened two independent real BTCUSDC `bookTicker` WebSocket sessions, received real non-crossed BBO, then requested live `https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDC&limit=1000`. Binance returned `451 Unavailable For Legal Reasons` to the US-hosted runner; L2 was **not** bridged in Rust. Full evidence: https://github.com/xxxxxwater/pg-tsy-core-bootstrap/actions/runs/35343735329 . Do not retry through undocumented geographic or account workarounds, mark this as passing, or relax snapshot validity checks.
2. Separately, an ephemeral read-only probe on an authorized connected Ubuntu host successfully connected twice to real `wss://fstream.binance.com/public/ws/btcusdc@bookTicker` and fetched public REST BTCUSDC depth, HTTP 200 with 1,000 bids and 1,000 asks. It did not use private Binance credentials.
3. A second **Python** ephemeral read-only probe on that host opened real `btcusdc@depth@100ms`, buffered increments, fetched an actual Binance REST depth snapshot, validated bridge and **five contiguous depth updates** (`pu` matching previous `u` after bridge), applied decimal levels, checked nonempty and uncrossed sides. Output: `HOST_REAL_PUBLIC_L2_PASS symbol=BTCUSDC snapshot_id=11589717710199 final_id=11589717751536 validated_contiguous_diffs=5 bid_lt_ask=true elapsed_s=0.69`. This establishes one short network/sequence observation on the host, **not Rust runtime L2, restart recovery, or sustained operation**.

## Current boundaries (implementation is not acceptance)

| Component | Verified status | Blocking gap |
| --- | --- | --- |
| Telegram optional feature | CI passes | Live Telegram control + auth/role acceptance with representative commands and unauthorized actors not evidenced. |
| Rust public BBO WebSocket | Two authentic connections in GitHub probe | Automated supervisor long-run/reconnect injection and freshness gates not accepted. |
| Public BTCUSDC depth | Host Python real snapshot + 5 deltas | Actual Rust adapter L2 blocked by US runner REST 451; must run read-only Rust test from authorized host and verify gap/resnapshot and >24h session behavior. |
| Signed Binance PM REST client | Compiles and deterministic HMAC tests | No real authenticated read-side success, clock/recvWindow/error-code compatibility or signed test-order acknowledgment captured. |
| Execution adapter | Implementation exists; default mutation capability disabled | Not wired into production `pg-core`; no risk→durable intent→fencing→venue→OMS full chain or independent account symbol/mode validation. |
| Binance User Stream | Parsing/tracker and transport source exist | No actual PM listenKey/WS ORDER_TRADE_UPDATE, true fill ID/fee journal, expired-key recovery, or persisted replay proven. |
| Continuous reconciliation | Durable `reconcile_once` and read-side functions exist | No scheduled/observed production Binance reconciliation loop, complete paginated orders/userTrades import, ownership proof or reboot/lost-ACK test against venue truth. |
| JEV async advisory | Research client and Rust advisory contract tests | No proven asynchronous bridge in running Rust market→strategy→risk, no causally journaled provider response or fee-aware live performance. |
| Account safety and profitability | No private test conducted | Independent account-wide BTC collateral/maintenance truth, manual position non-adoption, protection checks, fees, and realized NET basis points unverified. |

## Minimum remaining release gates

1. On a permitted host and a separately isolated account/environment, run the **unmodified strict Rust** public probe. Record HTTP status, snapshot ID, WS `U/u/pu`, crossing, freshness, disconnect and fresh resnapshot evidence; replay gaps/failures. A passing Python probe cannot substitute.
2. With an operator-controlled secret store and separate explicit authorization, first verify read-only signed PM endpoints, account permissions, correct BTCUSDC contract, one-way mode, exact exchangeInfo filters, BTC collateral and manual-position ownership. Do not expose keys/signatures/listen keys in logs or GitHub Actions.
3. Wire one Binance adapter into an isolated staging Rust runtime through the existing risk gate, persisted intent, fencing and journal-before-POST. Explicitly authorize only a tiny, loss-bounded canary **after** testnet/shadow evidence and operator sign-off; otherwise never issue a real POST.
4. Capture the same durable 34-character OMS ID ↔ deterministic Binance 28-character ID across POST acknowledgment, signed query by client ID, order history, userTrades, and actual User Stream execution/trade/fee events. Validate partial fills/cancel races, duplicate and late trades, terminal orders absent from openOrders, account-wide reconciliation and no manual-position adoption.
5. Inject lost ACK, disconnection, expired listenKey, REST timeout/429/5xx, database loss, lease fencing loss, restart mid-fill and emergency reduce-only behavior. Failure or ambiguous state must block new exposure, not silently resubmit. Prove history pagination/watermarks and durable fill dedup across restart.
6. Only after independent reproducible, continuously monitored end-to-end logs with zero unexplained differences, documented on-call/kill-switch/rollback and explicit owner sign-off can an operator consider enabling the guarded live capability. Validate actual filled fee/slippage and realized net results rather than assuming a 1–8bps edge.

**No authenticated order was sent, no real User Stream was consumed, no production Rust daemon was deployed, and no unattended approval was granted as part of this record.**
