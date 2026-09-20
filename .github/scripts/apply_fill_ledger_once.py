from pathlib import Path


def patch(path: str, before: str, after: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(before)
    assert count == 1, f"expected one anchor in {path}; got {count}: {before[:90]!r}"
    file.write_text(text.replace(before, after, 1))


patch("rust/crates/pg-store/src/lib.rs", "use pg_oms::OrderRecord;", "pub mod fill_ledger;\n\nuse pg_oms::OrderRecord;")
patch(
    "rust/crates/pg-store/src/lib.rs",
    '        sqlx::raw_sql(include_str!("../migrations/0002_trading_state.sql"))\n            .execute(&self.pool)\n            .await?;\n        Ok(())',
    '        sqlx::raw_sql(include_str!("../migrations/0002_trading_state.sql"))\n            .execute(&self.pool)\n            .await?;\n        sqlx::raw_sql(include_str!("../migrations/0003_execution_fills.sql"))\n            .execute(&self.pool)\n            .await?;\n        Ok(())',
)
patch("rust/crates/pg-store/Cargo.toml", "serde_json.workspace = true", "serde.workspace = true\nserde_json.workspace = true\nrust_decimal.workspace = true")
patch("rust/adapters/pg-binance/src/lib.rs", "pub mod rest_transport;", "pub mod rest_transport;\npub mod trade_history;")
patch("rust/crates/pg-store/src/fill_ledger.rs", "use sqlx::Row;\n", "")
patch(
    "rust/adapters/pg-binance/src/rest_transport.rs",
    "    pub async fn position_risk(&self) -> Result<Value, ExecutionError> {",
    '''    /// Signed, bounded UM trade-history page. A short page does not prove older history coverage.
    /// Only commit a cursor after all rows are ownership-matched and journaled.
    pub async fn user_trades_page(
        &self,
        from_id: Option<u64>,
        limit: u16,
    ) -> Result<crate::trade_history::TradePage, ExecutionError> {
        if !(1..=1000).contains(&limit)
            || from_id.is_some_and(|id| id > i64::MAX as u64)
        {
            return Err(ExecutionError::Conversion("invalid UM history cursor or limit".into()));
        }
        let mut params = vec![
            ("symbol".into(), SYMBOL.into()),
            ("limit".into(), limit.to_string()),
        ];
        if let Some(id) = from_id {
            params.push(("fromId".into(), id.to_string()));
        }
        let raw = self.signed(Method::GET, TRADES_PATH, &params).await?;
        crate::trade_history::decode_trade_page(&raw, from_id, usize::from(limit))
            .map_err(|_| ExecutionError::Conversion("invalid UM trade-history evidence".into()))
    }

    pub async fn position_risk(&self) -> Result<Value, ExecutionError> {''',
)
ci = Path(".github/workflows/ci.yml")
text = ci.read_text()
assert "\n  postgres-fill-ledger:" not in text
text += '''
  postgres-fill-ledger:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env:
          POSTGRES_USER: pgtsy_ci
          POSTGRES_PASSWORD: only-for-ci
          POSTGRES_DB: pgtsy_fill_test
        ports: ['5432:5432']
        options: >-
          --health-cmd "pg_isready -U pgtsy_ci -d pgtsy_fill_test"
          --health-interval 5s
          --health-timeout 5s
          --health-retries 10
    env:
      PG_TEST_DATABASE_URL: postgres://pgtsy_ci:only-for-ci@localhost:5432/pgtsy_fill_test
    defaults:
      run:
        working-directory: rust
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.98.1
        with:
          components: rustfmt,clippy
      - run: cargo test -p pg-store --test fill_ledger_pg
      - run: cargo test -p pg-binance trade_history
      - run: cargo clippy -p pg-store -p pg-binance --all-targets -- -D warnings
'''
ci.write_text(text)
print("Integrated read-only signed history paging, fenced fill ledger, real PostgreSQL CI")
