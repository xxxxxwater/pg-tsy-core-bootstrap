# AGENTS.md — rules for human and AI contributors

This repository is designed to be maintained by one engineer with AI agents. Optimize for **correctness, explicit contracts and small diffs**, not cleverness.

## Non-negotiable invariants

1. Never let research code call exchange order endpoints.
2. Never bypass `pg-risk` for a new-exposure order.
3. Never infer strategy ownership from symbol/side/position alone.
4. Never treat an unknown venue state as success.
5. Never auto-adopt manual positions into strategy state.
6. Never weaken restart reconciliation to make a test pass.
7. Live trading defaults to disabled. Any live-enabling change requires explicit configuration and tests.
8. Reduce-only/emergency pathways must remain distinguishable from new-exposure pathways.
9. Contract changes under `contracts/` require compatibility tests in both Python and Rust.
10. A PR should normally modify one domain boundary at a time.

## Agent workflow

Before changing code:

- Read `docs/ARCHITECTURE.md` and relevant ADRs.
- Identify the owning module.
- Write/adjust a test that describes the intended behavior.
- Prefer extending an interface over cross-module imports.
- Keep production exchange adapters thin; domain logic belongs in core crates.

Before claiming completion:

- Python: `ruff check`, `pytest`.
- Rust: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- If execution/reconcile/journal changed, run replay/failure tests.
- State exactly what was not validated against a real venue.

## Forbidden shortcuts

- No `except Exception: pass` / ignored Rust errors in trading paths.
- No unbounded retry loops.
- No floating-point money/quantity in the live core when venue precision matters.
- No credentials in repo, fixtures or logs.
- No direct database mutation to repair trading state without a journaled operational procedure.
