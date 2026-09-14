.PHONY: check python-check rust-check test rust-docker docker-up docker-down docker-status docker-logs health ready metrics reload strategy-validate strategy-replay policy-replay

# Strategy definitions used by the validation and replay targets. Point this at
# data/replay/strategies to replay the shipped fixtures.
STRATEGY_DIR ?= ../strategies

check: python-check rust-check

python-check:
	cd research && python -m compileall -q src tests
	cd research && if command -v ruff >/dev/null 2>&1; then ruff check .; fi
	cd research && if command -v pytest >/dev/null 2>&1; then pytest -q; fi

rust-check:
	@if command -v cargo >/dev/null 2>&1; then cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace; else echo 'cargo not installed; skipping Rust checks'; fi

test: check

# Run the Rust checks in the pinned toolchain container. Useful on hosts without
# a local Rust toolchain; keeps results identical to CI and to the Dockerfile.
rust-docker:
	pwsh ./scripts/rust-docker.ps1 "fmt --check"
	pwsh ./scripts/rust-docker.ps1 "clippy --workspace --all-targets -- -D warnings"
	pwsh ./scripts/rust-docker.ps1 "test --workspace"

strategy-validate:
	cd rust && PG_INSTANCE_ID=local-strategy-validate PG_STRATEGY_DIR=$(STRATEGY_DIR) cargo run -p pg-core

strategy-replay:
	@if [ -z "$(EVENTS)" ]; then echo 'usage: make strategy-replay EVENTS=path/to/events.jsonl [STRATEGY_DIR=../data/replay/strategies]'; exit 2; fi
	cd rust && PG_INSTANCE_ID=local-strategy-replay PG_STRATEGY_DIR=$(STRATEGY_DIR) cargo run -p pg-core -- --replay-market-events ../$(EVENTS)

policy-replay:
	@if [ -z "$(FEATURES)" ]; then echo 'usage: make policy-replay FEATURES=path/to/features.jsonl [STRATEGY_DIR=../data/replay/strategies]'; exit 2; fi
	cd rust && PG_INSTANCE_ID=local-policy-replay PG_STRATEGY_DIR=$(STRATEGY_DIR) cargo run -p pg-core -- --replay-policy-features ../$(FEATURES)

docker-up:
	docker compose up -d --build

docker-status:
	docker compose ps

docker-logs:
	docker compose logs -f --tail=200 pg-core

health:
	curl --fail --silent http://127.0.0.1:$${PG_HEALTH_PORT:-8080}/healthz; echo

ready:
	curl --fail --silent http://127.0.0.1:$${PG_HEALTH_PORT:-8080}/readyz; echo

# Ask the running daemon to re-validate and swap strategy definitions in place.
# The operator surface can never submit, cancel or flatten anything directly.
reload:
	curl --fail --silent --request POST http://127.0.0.1:$${PG_HEALTH_PORT:-8080}/admin/reload; echo

metrics:
	curl --fail --silent http://127.0.0.1:$${PG_HEALTH_PORT:-8080}/metrics

docker-down:
	docker compose down
