use anyhow::{Context, Result, bail};
use pg_marketdata::FeedKind;
use pg_observability::{
    FeedSnapshot, ObservabilityConfig, ObservabilityServer, RuntimeObservatory, VenueSnapshot,
};
use pg_risk::{RiskLimits, evaluate_signal};
use pg_runtime::{GateStatus, RunConfig, RunMode, StartupGate};
use pg_strategy::StrategyDecision;
use pg_strategy::policy::{FeatureFrame, PositionView};
use pg_strategy::registry::StrategyRegistry;
use pg_types::{AssetKey, RiskDecision, Signal, Venue};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
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

fn build_version() -> String {
    env::var("PG_BUILD_VERSION").unwrap_or_else(|_| {
        option_env!("GIT_COMMIT_SHA")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_string()
    })
}

fn venue_id(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "binance-pm",
        Venue::Hyperliquid => "hyperliquid",
        Venue::InteractiveBrokers => "ibkr",
    }
}

fn feed_name(kind: &FeedKind) -> String {
    match kind {
        FeedKind::Trades => "TRADES".into(),
        FeedKind::BestBidAsk => "BBO".into(),
        FeedKind::L2Book => "L2".into(),
        FeedKind::Candle { interval_ns } => format!("CANDLE:{interval_ns}"),
    }
}

fn configured_runtime_inventory(
    registry: &StrategyRegistry,
) -> (Vec<FeedSnapshot>, Vec<VenueSnapshot>) {
    let mut feed_keys = BTreeSet::new();
    let mut feeds = Vec::new();
    let mut venue_ids = BTreeSet::new();

    for spec in registry
        .subscriptions()
        .into_iter()
        .chain(registry.policy_subscriptions())
    {
        let venue = venue_id(spec.venue).to_string();
        let feed = feed_name(&spec.kind);
        let key = (venue.clone(), spec.asset.clone(), feed.clone());
        if !feed_keys.insert(key) {
            continue;
        }
        venue_ids.insert(venue.clone());
        feeds.push(FeedSnapshot {
            venue,
            feed,
            asset: spec.asset,
            status: "PENDING".into(),
            age_ms: None,
            required: true,
        });
    }

    let venues = venue_ids
        .into_iter()
        .map(|id| VenueSnapshot {
            id,
            enabled: true,
            market_data: "PENDING".into(),
            execution: "UNKNOWN".into(),
            reconcile: "UNKNOWN".into(),
            latency_ms: None,
        })
        .collect();
    (feeds, venues)
}

async fn serve_runtime_observability(
    config: RunConfig,
    registry: Option<StrategyRegistry>,
) -> Result<()> {
    let observability_config =
        ObservabilityConfig::from_env().context("invalid runtime observability configuration")?;
    let observatory = RuntimeObservatory::new(
        &config,
        build_version(),
        observability_config.stale_after_ms,
        observability_config.event_capacity,
    );
    observatory.record_event(
        "runtime.config_loaded",
        "info",
        format!(
            "runtime configured environment={} mode={:?} real_venue={}",
            config.environment,
            config.mode,
            config.routes_to_real_venue()
        ),
        None,
        None,
    );

    if let Some(registry) = registry.as_ref() {
        let strategy_ids = registry.strategy_ids();
        let (feeds, venues) = configured_runtime_inventory(registry);
        let strategy_count = strategy_ids.len();
        let feed_count = feeds.len();
        observatory.set_strategy_inventory(strategy_ids);
        observatory.set_configured_feeds(feeds);
        observatory.set_venues(venues);
        observatory.set_startup_gate(StartupGate::StrategyAllowlistLoaded, GateStatus::Passed);
        observatory.record_event(
            "strategy.inventory_loaded",
            "info",
            format!("loaded {strategy_count} strategies requiring {feed_count} unique feeds"),
            None,
            None,
        );
    } else {
        observatory.set_startup_gate(
            StartupGate::StrategyAllowlistLoaded,
            GateStatus::Failed("no strategy directory found".into()),
        );
        observatory.record_event(
            "strategy.inventory_missing",
            "warning",
            "no strategy directory found; strategy allowlist gate remains closed",
            None,
            None,
        );
    }

    if config.mode == RunMode::Live && config.routes_to_real_venue() {
        observatory.set_startup_gate(
            StartupGate::LiveTradingExplicitlyEnabled,
            GateStatus::Passed,
        );
    }

    let server = ObservabilityServer::spawn(observability_config, observatory.clone())
        .await
        .context("failed to bind runtime observability server")?;
    observatory.record_event(
        "observability.listening",
        "info",
        format!("runtime observability listening on {}", server.local_addr()),
        None,
        None,
    );
    tracing::info!(
        address = %server.local_addr(),
        "runtime observability API ready"
    );
    println!(
        "pg-core observability is serving real runtime evidence on http://{}; until lease/storage/feed/execution/reconcile components report healthy evidence, new exposure remains fail-closed",
        server.local_addr()
    );

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;
    observatory.record_event(
        "runtime.shutdown_requested",
        "info",
        "operator/process shutdown requested",
        None,
        None,
    );
    server.stop().await;
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

    if args.is_empty() {
        return serve_runtime_observability(config, registry).await;
    }

    let path = &args[0];
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
