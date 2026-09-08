.PHONY: check python-check rust-check test docker-up docker-down strategy-validate strategy-replay

check: python-check rust-check

python-check:
	cd research && python -m compileall -q src tests
	cd research && if command -v ruff >/dev/null 2>&1; then ruff check .; fi
	cd research && if command -v pytest >/dev/null 2>&1; then pytest -q; fi

rust-check:
	@if command -v cargo >/dev/null 2>&1; then cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace; else echo 'cargo not installed; skipping Rust checks'; fi

test: check

strategy-validate:
	cd rust && PG_INSTANCE_ID=local-strategy-validate PG_STRATEGY_DIR=../strategies cargo run -p pg-core

strategy-replay:
	@if [ -z "$(EVENTS)" ]; then echo 'usage: make strategy-replay EVENTS=path/to/events.jsonl'; exit 2; fi
	cd rust && PG_INSTANCE_ID=local-strategy-replay PG_STRATEGY_DIR=../strategies cargo run -p pg-core -- --replay-market-events ../$(EVENTS)

docker-up:
	docker compose up -d postgres

docker-down:
	docker compose down
