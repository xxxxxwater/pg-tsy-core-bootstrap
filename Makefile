.PHONY: check python-check rust-check test docker-up docker-down

check: python-check rust-check

python-check:
	cd research && python -m compileall -q src tests
	cd research && if command -v ruff >/dev/null 2>&1; then ruff check .; fi
	cd research && if command -v pytest >/dev/null 2>&1; then pytest -q; fi

rust-check:
	@if command -v cargo >/dev/null 2>&1; then cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace; else echo 'cargo not installed; skipping Rust checks'; fi

test: check

docker-up:
	docker compose up -d postgres

docker-down:
	docker compose down
