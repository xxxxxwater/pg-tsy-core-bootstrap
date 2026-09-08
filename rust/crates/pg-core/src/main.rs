use anyhow::{Context, Result};
use pg_risk::{evaluate_signal, RiskLimits};
use pg_types::{RiskDecision, Signal};
use std::{env, fs, time::{SystemTime, UNIX_EPOCH}};

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos() as u64
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let path = env::args().nth(1).context("usage: pg-core <signal.json>")?;
    let signal: Signal = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut limits = RiskLimits::default();
    limits.allow_new_exposure = env::var("PG_LIVE_TRADING").ok().as_deref() == Some("true");
    let decision = evaluate_signal(&signal, now_ns(), &limits);
    match decision {
        RiskDecision::Allow => println!("signal accepted for further risk/order-intent processing: {}", signal.signal_id),
        RiskDecision::Reject { code, reason } => println!("signal rejected: {code}: {reason}"),
    }
    Ok(())
}
