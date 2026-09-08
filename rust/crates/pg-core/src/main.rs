use anyhow::{Context, Result};
use pg_risk::{RiskLimits, evaluate_signal};
use pg_runtime::RunConfig;
use pg_types::{RiskDecision, Signal};
use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos() as u64
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let config = RunConfig::from_env().context("invalid runtime configuration")?;
    tracing::info!(
        environment = %config.environment,
        instance_id = %config.instance_id,
        mode = ?config.mode,
        real_venue = config.routes_to_real_venue(),
        "runtime configuration loaded"
    );

    let Some(path) = env::args().nth(1) else {
        println!(
            "pg-core configured in {:?} mode; orchestration startup gates are not yet wired in this binary",
            config.mode
        );
        return Ok(());
    };

    let signal: Signal = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut limits = RiskLimits::default();
    limits.allow_new_exposure = config.routes_to_real_venue();
    let decision = evaluate_signal(&signal, now_ns(), &limits);
    match decision {
        RiskDecision::Allow => println!(
            "signal accepted for further strategy/risk/order-intent processing: {}",
            signal.signal_id
        ),
        RiskDecision::Reject { code, reason } => println!("signal rejected: {code}: {reason}"),
    }
    Ok(())
}
