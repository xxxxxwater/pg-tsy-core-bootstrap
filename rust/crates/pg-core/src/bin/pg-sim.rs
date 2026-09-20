use anyhow::{Context, Result};
use pg_execution::matching::{SessionPhase, TopOfBook, match_order};
use pg_types::advanced_order::AdvancedOrderIntent;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

#[derive(Debug, Deserialize)]
struct SimRequest {
    request_id: u64,
    order: AdvancedOrderIntent,
    top: TopOfBook,
    #[serde(default)]
    position: Decimal,
    now_ns: u64,
    #[serde(default = "default_session")]
    session: SessionPhase,
}

#[derive(Debug, Serialize)]
struct SimResponse {
    request_id: u64,
    result: pg_execution::matching::MatchResult,
}

fn default_session() -> SessionPhase {
    SessionPhase::Continuous
}

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());

    for (index, line) in stdin.lock().lines().enumerate() {
        let line = line.with_context(|| format!("failed to read request line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let request: SimRequest = serde_json::from_str(&line)
            .with_context(|| format!("invalid simulator request at line {}", index + 1))?;
        let result = match_order(
            &request.order,
            request.top,
            request.position,
            request.now_ns,
            request.session,
        );
        serde_json::to_writer(
            &mut stdout,
            &SimResponse {
                request_id: request.request_id,
                result,
            },
        )?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }

    Ok(())
}
