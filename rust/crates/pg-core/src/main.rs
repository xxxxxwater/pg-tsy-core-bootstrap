use anyhow::{Context, Result};
use pg_risk::{RiskLimits, evaluate_signal};
use pg_runtime::RunConfig;
use pg_strategy::registry::StrategyRegistry;
use pg_types::{RiskDecision, Signal};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos() as u64
}

fn resolve_strategy_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("PG_STRATEGY_DIR") {
        return Some(PathBuf::from(path));
    }
    for candidate in [Path::new("strategies"), Path::new("../strategies")] {
        if candidate.is_dir() {
            return Some(candidate.to_path_buf());
        }
    }
    None
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

    if let Some(strategy_dir) = resolve_strategy_dir() {
        let registry = StrategyRegistry::load_dir(&strategy_dir).with_context(|| {
            format!(
                "failed to load strategy definitions from {}",
                strategy_dir.display()
            )
        })?;
        tracing::info!(
            strategy_dir = %strategy_dir.display(),
            strategy_count = registry.len(),
            subscription_count = registry.subscriptions().len(),
            strategies = ?registry.strategy_ids(),
            "strategy definitions loaded"
        );
    } else {
        tracing::info!("no strategy directory found; set PG_STRATEGY_DIR to load definitions");
    }

    let Some(path) = env::args().nth(1) else {
        println!(
            "pg-core configured in {:?} mode; strategy definitions are validated at startup, while full market-data/execution orchestration remains a P0 runtime milestone",
            config.mode
        );
        return Ok(());
    };

    let signal: Signal = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let limits = RiskLimits {
        allow_new_exposure: config.routes_to_real_venue(),
        ..RiskLimits::default()
    };
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
