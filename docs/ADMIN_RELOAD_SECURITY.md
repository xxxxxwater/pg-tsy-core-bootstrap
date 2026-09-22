# `pg-core` HTTP admin reload: authenticated, fail-closed contract

Scope: `POST /admin/reload` in `rust/crates/pg-core/src/health.rs`. This operation enqueues `ControlCommand::ReloadStrategies`; it does **not** start trading, clear SAFE_HOLD, submit/cancel orders or flatten positions. This document does not certify the Telegram emergency path or real-venue acceptance.

## Default and authorization

- Without `PG_ADMIN_TOKEN`, or with an invalid token, reload responds **403 Forbidden** and does not enqueue any command. `/healthz`, `/readyz` and `/metrics` remain readable for monitoring.
- A configured token must be **32–512 printable ASCII characters without spaces or newlines**. Supply an independently generated high-entropy value through the process secret manager; never commit it, print it in CI, place it in an issue or pass it on a command line. Invalid configuration disables reload instead of falling back to a known token.
- With a valid token, `POST /admin/reload` requires exactly one `Authorization: Bearer <token>` header in a complete HTTP header block. No/wrong/malformed/duplicate authorization gets **401 Unauthorized**; non-POST gets **405 Method Not Allowed**. Valid authorization enqueues only reload (202) or reports that the control channel is unavailable (503).
- Restart the `pg-core` process to rotate the token: it is read at listener startup. Rotation/restart must follow an operator-approved maintenance procedure and must never silently enable trading.

## Operational boundaries

The existing Compose templates do **not** forward `PG_ADMIN_TOKEN` into the container: their admin endpoint therefore stays disabled by default. An operator needing reload must deliberately inject the secret into **the pg-core process environment** through a deployment-managed secret configuration (not into committed YAML, `.env.example`, GitHub Actions or a pasted Compose config dump), then ensure the listener is reachable only on a trusted management path. A token alone is not a substitute for network isolation or TLS when traversing untrusted networks. The existing listener defaults to `0.0.0.0:8080`; production Compose binds the published host port to `127.0.0.1` but does not isolate other containers sharing its Docker bridge network.

Example invocation *only from an already-trusted management shell with an already-injected secret*:

```bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer ${PG_ADMIN_TOKEN}" \
  http://127.0.0.1:8080/admin/reload
```

Do not enable this operation on a publicly exposed plaintext HTTP listener. Prefer a private network with authenticated TLS reverse proxy or local tunnel, and protect process environment inspection. For multi-user or remotely operated deployments, implement granular identities, audit trails, replay controls and rate limits before calling the entire control plane production-ready.

## Regression proof

`health.rs` contains token validation, HTTP parser tests and loopback TCP tests asserting no control message on absent/wrong tokens, a single reload message on valid authorization, and unaffected anonymous health reads. Run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test -p pg-core --bin pg-core health::tests` from `rust/`. CI results must match the final exact merge SHA; a passing unit test does not prove external account safety or a secure network deployment.
