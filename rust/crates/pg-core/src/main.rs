mod daemon;
mod health;

use anyhow::{Context, Result, bail};
use pg_risk::{RiskLimits, evaluate_signal};
use pg_runtime::RunConfig;
use pg_strategy::StrategyDecision;
use pg_strategy::policy::{FeatureFrame, PositionView};
use pg_strategy::registry::StrategyRegistry;
use pg_types::{AssetKey, RiskDecision, Signal};
use serde::Deserialize;
use serde_json::json;
use std::{
    env, fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
struct PolicyReplayFrame {
    instrument: AssetKey,
    #[serde(default)]
    features: FeatureFrame,
    #[serde(default)]
    position: PositionView,
    now_ns: u64,
}

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

fn load_strategy_registry() -> Result<Option<StrategyRegistry>> {
    let Some(strategy_dir) = resolve_strategy_dir() else {
        tracing::info!("no strategy directory found; set PG_STRATEGY_DIR to load definitions");
        return Ok(None);
    };
    let registry = StrategyRegistry::load_dir(&strategy_dir).with_context(|| {
        format!(
            "failed to load strategy definitions from {}",
            strategy_dir.display()
        )
    })?;
    tracing::info!(
        strategy_dir = %strategy_dir.display(),
        strategy_count = registry.len(),
        policy_count = registry.policy_count(),
        subscription_count = registry.subscriptions().len(),
        policy_subscription_count = registry.policy_subscriptions().len(),
        strategies = ?registry.strategy_ids(),
        "strategy definitions loaded"
    );
    Ok(Some(registry))
}

fn replay_market_events(registry: &mut StrategyRegistry, path: &Path) -> Result<()> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open market-event replay {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut event_count = 0_u64;
    let mut signal_count = 0_u64;
    let mut decision_count = 0_u64;
    let mut policy_frame_count = 0_u64;
    let position = PositionView::default();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read replay line {}", index + 1))?;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let event: pg_marketdata::MarketEvent = serde_json::from_str(&line)
            .with_context(|| format!("invalid MarketEvent JSON at line {}", index + 1))?;
        event_count = event_count.saturating_add(1);

        for routed in registry.route_event(&event) {
            if routed.output.signal.is_some() {
                signal_count = signal_count.saturating_add(1);
            }
            let decision = match routed.output.decision {
                StrategyDecision::Noop => None,
                StrategyDecision::Submit(intent) => {
                    decision_count = decision_count.saturating_add(1);
                    Some(json!({
                        "type": "submit",
                        "intent": intent,
                    }))
                }
                StrategyDecision::Hold(reason) => {
                    decision_count = decision_count.saturating_add(1);
                    Some(json!({
                        "type": "hold",
                        "reason": reason,
                    }))
                }
            };

            if routed.output.signal.is_some() || decision.is_some() {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "path": "legacy_automation",
                        "strategy_id": routed.strategy_id,
                        "factors": routed.output.factors,
                        "entry_filter": routed.output.entry_filter,
                        "signal": routed.output.signal,
                        "decision": decision,
                    }))?
                );
            }
        }

        for routed in registry.route_live_policy_event(&event, &position) {
            policy_frame_count = policy_frame_count.saturating_add(1);
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "path": "portable_policy_live",
                    "strategy_id": routed.strategy_id,
                    "instrument": routed.instrument,
                    "features": routed.features,
                    "entry": routed.entry,
                    "exit": routed.exit,
                }))?
            );
        }
    }

    eprintln!(
        "replay complete: events={event_count} signals={signal_count} non_noop_decisions={decision_count} portable_policy_frames={policy_frame_count}"
    );
    Ok(())
}

fn replay_policy_features(registry: &StrategyRegistry, path: &Path) -> Result<()> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open policy replay {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut frame_count = 0_u64;
    let mut routed_count = 0_u64;

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read replay line {}", index + 1))?;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let frame: PolicyReplayFrame = serde_json::from_str(&line)
            .with_context(|| format!("invalid PolicyReplayFrame JSON at line {}", index + 1))?;
        frame_count = frame_count.saturating_add(1);

        for routed in registry.route_policy_frame(
            &frame.instrument,
            &frame.features,
            &frame.position,
            frame.now_ns,
        ) {
            routed_count = routed_count.saturating_add(1);
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "path": "portable_policy_fixture",
                    "strategy_id": routed.strategy_id,
                    "instrument": routed.instrument,
                    "entry": routed.entry,
                    "exit": routed.exit,
                }))?
            );
        }
    }

    eprintln!("policy replay complete: frames={frame_count} routed={routed_count}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let config = RunConfig::from_env().context("invalid runtime configuration")?;
    tracing::info!(
        environment = %config.environment,
        instance_id = %config.instance_id,
        mode = ?config.mode,
        real_venue = config.routes_to_real_venue(),
        "runtime configuration loaded"
    );

    let mut registry = load_strategy_registry()?;
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.first().map(String::as_str) == Some("--serve") {
        if args.len() != 1 {
            bail!("usage: pg-core --serve");
        }
        let registry = registry
            .take()
            .context("--serve requires a strategy directory")?;
        return daemon::serve(config, registry).await;
    }

    if args.first().map(String::as_str) == Some("--replay-market-events") {
        if args.len() != 2 {
            bail!("usage: pg-core --replay-market-events <events.jsonl>");
        }
        let registry = registry
            .as_mut()
            .context("market-event replay requires a strategy directory")?;
        return replay_market_events(registry, Path::new(&args[1]));
    }

    if args.first().map(String::as_str) == Some("--replay-policy-features") {
        if args.len() != 2 {
            bail!("usage: pg-core --replay-policy-features <features.jsonl>");
        }
        let registry = registry
            .as_ref()
            .context("policy replay requires a strategy directory")?;
        return replay_policy_features(registry, Path::new(&args[1]));
    }

    let Some(path) = args.first() else {
        println!(
            "pg-core configured in {:?} mode. Use --serve for the fail-closed shadow daemon, --replay-market-events <events.jsonl> for normalized live-feature replay, or --replay-policy-features <features.jsonl> for fixture parity. Real-money live orchestration remains disabled until release gates are complete.",
            config.mode
        );
        return Ok(());
    };

    let signal: Signal = serde_json::from_str(&fs::read_to_string(path)?)?;
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
