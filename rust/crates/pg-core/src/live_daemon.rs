use crate::{
    dynamic_universe::{DynamicUniverseResolution, refresh_dynamic_universe},
    health::{ControlCommand, HealthSnapshot, HealthState},
    secrets::{bool_env, optional_secret, required_secret, required_value},
};
use anyhow::{Context, Result, bail};
use pg_execution::{CompositeExecutionAdapter, ExecutionAdapter};
use pg_marketdata::{
    FeedSpec, InstrumentDescriptor, MarketDataSource, MarketEvent, SubscriptionSupervisor,
};
use pg_oms::OrderRecord;
use pg_orchestrator::{AdapterRegistry, DurableExecution};
use pg_reconcile::{Ownership, VenuePosition};
use pg_risk::{RiskLimits, evaluate_order};
use pg_runtime::{RunConfig, RunMode, ShutdownPolicy, StartupChecklist, StartupGate};
use pg_store::{LeaseHealth, PostgresStore};
use pg_strategy::{
    StrategyDecision,
    definition::PolicyInstance,
    policy::{
        PositionView,
        graph::{EntryPolicyDecision, ExitPolicyDecision},
    },
    registry::StrategyRegistry,
};
use pg_types::{AssetKey, ExposureEffect, OrderIntent, RiskDecision, Side, Venue};
use rust_decimal::Decimal;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::MissedTickBehavior,
};

#[cfg(feature = "hyperliquid-marketdata")]
use pg_hyperliquid::{
    HyperliquidExecutionAdapter, HyperliquidExecutionConfig, HyperliquidMarketDataSource,
    HyperliquidNetwork,
};
#[cfg(feature = "ibkr-marketdata")]
use pg_ibkr::{
    IbkrConfig, IbkrExecutionAdapter, IbkrExecutionConfig, IbkrMarketDataSource, IbkrStockSpec,
};

#[derive(Debug)]
struct FeedFatal {
    spec: FeedSpec,
    reason: String,
}

#[derive(Default)]
struct FeedTaskManager {
    tasks: BTreeMap<String, JoinHandle<()>>,
    next_sequence: usize,
}

impl FeedTaskManager {
    fn sync(
        &mut self,
        feeds: Vec<FeedSpec>,
        supervisor: &mut SubscriptionSupervisor,
        sink: &mpsc::Sender<MarketEvent>,
        fatal: &mpsc::Sender<FeedFatal>,
    ) -> Result<()> {
        let desired = feeds
            .iter()
            .cloned()
            .map(|spec| (feed_key(&spec), spec))
            .collect::<BTreeMap<_, _>>();

        let removed = self
            .tasks
            .keys()
            .filter(|key| !desired.contains_key(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in removed {
            if let Some(task) = self.tasks.remove(&key) {
                task.abort();
            }
        }

        supervisor.apply_derived_feeds(feeds);
        for (key, spec) in desired {
            if self.tasks.contains_key(&key) {
                continue;
            }
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.saturating_add(1);
            let task = spawn_live_feed(sequence, spec, sink.clone(), fatal.clone())?;
            self.tasks.insert(key, task);
        }
        Ok(())
    }

    fn restart_failed(
        &mut self,
        spec: FeedSpec,
        supervisor: &mut SubscriptionSupervisor,
        sink: &mpsc::Sender<MarketEvent>,
        fatal: &mpsc::Sender<FeedFatal>,
    ) -> Result<()> {
        let key = feed_key(&spec);
        self.tasks.remove(&key);
        if supervisor.status(&spec).is_none() {
            return Ok(());
        }
        supervisor.mark_reconnecting(&spec);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let task = spawn_live_feed(sequence, spec, sink.clone(), fatal.clone())?;
        self.tasks.insert(key, task);
        Ok(())
    }

    fn stop_all(&mut self) {
        for (_, task) in std::mem::take(&mut self.tasks) {
            task.abort();
        }
    }
}

#[cfg(feature = "ibkr-marketdata")]
struct DynamicIbkrExecution {
    composite: Arc<CompositeExecutionAdapter>,
    base: IbkrConfig,
    next_client_id: i32,
    allow_software_reduce_only: bool,
}

#[cfg(feature = "ibkr-marketdata")]
impl DynamicIbkrExecution {
    async fn sync_assets(
        &mut self,
        desired: &BTreeSet<String>,
        descriptors: &BTreeMap<AssetKey, InstrumentDescriptor>,
    ) -> Result<()> {
        let existing = self.composite.assets().into_iter().collect::<BTreeSet<_>>();
        for asset in desired.difference(&existing) {
            let mut config = self.base.clone();
            config.client_id = self.next_client_id;
            self.next_client_id = self
                .next_client_id
                .checked_add(1)
                .context("IBKR execution client id overflow")?;
            let spec = ibkr_stock_spec_with_descriptor(asset, descriptors);
            let adapter = IbkrExecutionAdapter::connect(IbkrExecutionConfig {
                ibkr: config,
                instrument: spec,
                submit_ack_timeout_ms: env_u64("IBKR_SUBMIT_ACK_TIMEOUT_MS", 5_000)?,
                cancel_ack_timeout_ms: env_u64("IBKR_CANCEL_ACK_TIMEOUT_MS", 5_000)?,
                allow_software_reduce_only: self.allow_software_reduce_only,
            })
            .await
            .with_context(|| format!("failed to initialize IBKR execution adapter for {asset}"))?;
            self.composite
                .register_instrument(asset.clone(), Arc::new(adapter));
        }

        // Keep one account-capable child alive even when the scanner temporarily
        // yields no strategy assets. It is read-only unless its symbol is desired.
        let current = self.composite.assets();
        let removable = current
            .iter()
            .filter(|asset| !desired.contains(*asset))
            .cloned()
            .collect::<Vec<_>>();
        for asset in removable {
            if self.composite.len() <= 1 {
                break;
            }
            self.composite.remove_instrument(&asset);
        }
        Ok(())
    }
}

struct BuiltAdapters {
    registry: AdapterRegistry,
    #[cfg(feature = "ibkr-marketdata")]
    ibkr_dynamic: Option<DynamicIbkrExecution>,
}

#[derive(Default)]
struct RuntimePins {
    strategy: BTreeSet<AssetKey>,
    operational: BTreeSet<AssetKey>,
    safe_hold: BTreeSet<AssetKey>,
    strategy_positions: BTreeMap<AssetKey, Decimal>,
}

/// Production/paper runtime. No production path constructs or falls back to a
/// simulated execution adapter.
pub async fn serve(config: RunConfig, mut registry: StrategyRegistry) -> Result<()> {
    if config.mode == RunMode::Shadow {
        bail!("live_daemon refuses PG_RUN_MODE=shadow");
    }
    if registry.is_empty() && !registry.has_dynamic_templates() {
        bail!("real-venue daemon requires at least one enabled strategy definition");
    }
    if config.mode == RunMode::Live && !config.live_trading_enabled {
        bail!("live mode requires PG_LIVE_TRADING=true");
    }

    let dynamic_enabled = registry.has_dynamic_templates();
    let empty_pins = BTreeSet::new();
    let mut dynamic_resolution = if dynamic_enabled {
        refresh_dynamic_universe(&mut registry, &empty_pins, &empty_pins)
            .await
            .context("initial dynamic universe discovery failed")?
    } else {
        DynamicUniverseResolution::default()
    };
    if registry.is_empty() {
        bail!("real-venue daemon has no strategy instances after universe discovery");
    }

    let mut feeds = registry.subscriptions();
    if feeds.is_empty() {
        bail!("enabled strategies derived zero market-data subscriptions");
    }
    validate_live_feeds(&feeds)?;

    let database_url = env::var("PG_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .context("PG_DATABASE_URL (or DATABASE_URL) is required for --serve")?;
    let store = Arc::new(PostgresStore::connect(&database_url, 8).await?);
    store.migrate().await?;
    store.ping().await?;

    let mut checklist = StartupChecklist::new();
    checklist.pass(StartupGate::DatabaseReachable);
    checklist.pass(StartupGate::StrategyAllowlistLoaded);

    let mode_name = match config.mode {
        RunMode::Paper => "paper",
        RunMode::Live => "live",
        RunMode::Shadow => unreachable!(),
    };
    let lease_key = env::var("PG_LEASE_KEY")
        .unwrap_or_else(|_| format!("pg-core:{}:{mode_name}", config.environment));
    let lease = store
        .acquire_lease(
            &lease_key,
            &config.instance_id,
            config.lease_ttl_seconds as i32,
        )
        .await?;
    checklist.pass(StartupGate::RuntimeLeaseAcquired);

    let configured_venues = configured_live_venues(&registry, &feeds, &dynamic_resolution.operational_pins);
    let BuiltAdapters {
        registry: adapters,
        #[cfg(feature = "ibkr-marketdata")]
        mut ibkr_dynamic,
    } = build_real_adapter_registry(
        &feeds,
        &dynamic_resolution.operational_pins,
        &dynamic_resolution.descriptors,
        &configured_venues,
        &config,
    )
    .await?;
    let venues = adapters.venues();
    if venues.is_empty() {
        bail!("no real execution adapters were registered for the enabled strategies");
    }
    checklist.pass(StartupGate::VenueAuthenticated);

    let execution = Arc::new(DurableExecution::new(
        store.clone(),
        lease.clone(),
        adapters,
    ));
    store
        .append_event(
            &format!("runtime:{}", config.instance_id),
            "runtime.boot",
            &json!({
                "environment": &config.environment,
                "instance_id": &config.instance_id,
                "mode": mode_name,
                "execution": "real_venue",
                "venues": &venues,
                "dynamic_universe": dynamic_enabled,
                "fencing_token": lease.fencing_token,
            }),
            lease.fencing_token,
        )
        .await?;
    checklist.pass(StartupGate::JournalWritable);

    if config.mode == RunMode::Live && config.live_trading_enabled {
        checklist.pass(StartupGate::LiveTradingExplicitlyEnabled);
    } else if config.mode == RunMode::Live {
        checklist.fail(
            StartupGate::LiveTradingExplicitlyEnabled,
            "PG_LIVE_TRADING is false",
        );
    } else {
        checklist.pass(StartupGate::LiveTradingExplicitlyEnabled);
    }

    let mut latest_positions = BTreeMap::<AssetKey, VenuePosition>::new();
    let mut latest_orders = BTreeMap::<String, OrderRecord>::new();
    let mut reconcile_safe_hold = BTreeSet::new();
    let mut startup_clean = true;
    let mut ambiguous_clean = true;
    for venue in venues.iter().copied() {
        let recovery = execution.recover_ambiguous(venue).await?;
        for item in &recovery.items {
            if !item.resolved {
                ambiguous_clean = false;
                reconcile_safe_hold.insert(AssetKey::new(item.venue, item.asset.clone()));
            }
        }
        let cycle = execution.reconcile_once(venue).await?;
        if !cycle.report.clean() {
            startup_clean = false;
            reconcile_safe_hold.extend(cycle.report.safe_hold_assets.iter().cloned());
        }
        for order in cycle.orders {
            latest_orders.insert(order.client_order_id.clone(), order);
        }
        for position in cycle.positions {
            latest_positions.insert(position.key(), position);
        }
    }
    let mut runtime_pins = derive_runtime_pins(
        &latest_positions,
        &latest_orders,
        &reconcile_safe_hold,
    );

    if dynamic_enabled {
        dynamic_resolution = refresh_dynamic_universe(
            &mut registry,
            &runtime_pins.strategy,
            &runtime_pins.operational,
        )
        .await
        .context("post-reconcile dynamic universe pinning failed")?;
        feeds = registry.subscriptions();
        validate_live_feeds(&feeds)?;
        ensure_venues_registered(&feeds, &venues)?;
        #[cfg(feature = "ibkr-marketdata")]
        if let Some(dynamic) = ibkr_dynamic.as_mut() {
            let desired = desired_ibkr_assets(&feeds, &dynamic_resolution.operational_pins);
            dynamic
                .sync_assets(&desired, &dynamic_resolution.descriptors)
                .await?;
        }
    }

    checklist.pass(StartupGate::OpenOrdersLoaded);
    checklist.pass(StartupGate::PositionsLoaded);
    if startup_clean {
        checklist.pass(StartupGate::OwnershipReconciled);
    } else {
        checklist.fail(
            StartupGate::OwnershipReconciled,
            "startup reconcile found venue/local drift or unknown position ownership",
        );
    }
    if ambiguous_clean {
        checklist.pass(StartupGate::UnknownStateClear);
    } else {
        checklist.fail(
            StartupGate::UnknownStateClear,
            "one or more ambiguous order outcomes remain unresolved",
        );
    }

    let auto_start = bool_env("PG_AUTO_START", config.mode == RunMode::Paper)?;
    let mut trading_started = auto_start && startup_clean && ambiguous_clean;
    let mut risk_limits = RiskLimits {
        allow_new_exposure: trading_started,
        ..RiskLimits::default()
    };

    let health_addr: SocketAddr = env::var("PG_HEALTH_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .context("invalid PG_HEALTH_ADDR")?;
    let health = HealthState::new(HealthSnapshot::booting(mode_name));
    health
        .mutate(|snapshot| {
            snapshot.lease_healthy = true;
            snapshot.feeds_total = feeds.len();
            snapshot.blocking_gates = blocking_gate_names(&checklist, config.mode);
        })
        .await;
    let (control_tx, mut control_rx) = mpsc::channel::<ControlCommand>(8);
    tokio::spawn(crate::health::serve(
        health_addr,
        health.clone(),
        Some(control_tx),
    ));

    let heartbeat = store.spawn_lease_heartbeat(lease.clone());
    let mut lease_health = heartbeat.health();
    let (event_tx, mut event_rx) = mpsc::channel(16_384);
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<FeedFatal>(256);
    let mut supervisor = SubscriptionSupervisor::new();
    let mut feed_tasks = FeedTaskManager::default();
    feed_tasks.sync(
        feeds.clone(),
        &mut supervisor,
        &event_tx,
        &fatal_tx,
    )?;

    let max_staleness_ns = config.max_market_staleness_ms.saturating_mul(1_000_000);
    let reconcile_interval_ms = env_u64("PG_RECONCILE_INTERVAL_MS", 2_000)?;
    if !(250..=60_000).contains(&reconcile_interval_ms) {
        bail!("PG_RECONCILE_INTERVAL_MS must be in [250, 60000]");
    }
    let universe_refresh_seconds = env_u64("PG_UNIVERSE_REFRESH_SECONDS", 300)?;
    if !(15..=86_400).contains(&universe_refresh_seconds) {
        bail!("PG_UNIVERSE_REFRESH_SECONDS must be in [15, 86400]");
    }
    let mut reconcile_tick = tokio::time::interval(Duration::from_millis(reconcile_interval_ms));
    reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut universe_tick = tokio::time::interval(Duration::from_secs(universe_refresh_seconds));
    universe_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    universe_tick.tick().await;
    let mut health_tick = tokio::time::interval(Duration::from_secs(1));
    health_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!(
        mode = mode_name,
        execution_mode = "REAL_VENUE",
        ?venues,
        feeds = supervisor.feed_count(),
        strategies = registry.len(),
        policies = registry.policy_count(),
        dynamic_assets = dynamic_resolution.strategy_assets.len(),
        operational_pins = dynamic_resolution.operational_pins.len(),
        fencing_token = lease.fencing_token,
        trading_started,
        "real-venue strategy daemon started"
    );

    let strategy_dir = crate::resolve_strategy_dir();
    let mut live_policy_intents = BTreeMap::<String, (Venue, String)>::new();
    let mut orders_journaled = 0_u64;

    let result: Result<()> = loop {
        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(anyhow::anyhow!("all market-data event senders stopped"));
                };
                supervisor.observe(&event);
                let key = event_asset_key(&event);
                let position = position_view(
                    runtime_pins
                        .strategy_positions
                        .get(&key)
                        .copied()
                        .unwrap_or(Decimal::ZERO),
                );
                let mut decisions = 0_u64;

                for routed in registry.route_event(&event) {
                    let strategy_id = routed.strategy_id.clone();
                    match routed.output.decision {
                        StrategyDecision::Submit(intent) => {
                            if registry
                                .policy(&strategy_id)
                                .is_some_and(PolicyInstance::is_policy_driven)
                            {
                                continue;
                            }
                            decisions = decisions.saturating_add(1);
                            if submit_intent(
                                execution.as_ref(),
                                &risk_limits,
                                &runtime_pins.safe_hold,
                                &strategy_id,
                                &intent,
                            )
                            .await
                            .is_some()
                            {
                                orders_journaled = orders_journaled.saturating_add(1);
                            }
                        }
                        StrategyDecision::Hold(reason) => {
                            tracing::debug!(%strategy_id, %reason, "strategy hold");
                        }
                        StrategyDecision::Noop => {}
                    }
                }

                for routed in registry.route_live_policy_event(&event, &position) {
                    decisions = decisions.saturating_add(1);
                    if strategy_is_busy(
                        execution.as_ref(),
                        &mut live_policy_intents,
                        &routed.strategy_id,
                    )
                    .await
                    {
                        continue;
                    }
                    let Some(intent) = policy_intent(&registry, &routed, &position) else {
                        continue;
                    };
                    let strategy_id = routed.strategy_id.clone();
                    if let Some(client_order_id) = submit_intent(
                        execution.as_ref(),
                        &risk_limits,
                        &runtime_pins.safe_hold,
                        &strategy_id,
                        &intent,
                    )
                    .await
                    {
                        orders_journaled = orders_journaled.saturating_add(1);
                        live_policy_intents.insert(strategy_id, (intent.venue, client_order_id));
                    }
                }

                health.mutate(|snapshot| {
                    snapshot.events_total = snapshot.events_total.saturating_add(1);
                    snapshot.policy_decisions_total = snapshot
                        .policy_decisions_total
                        .saturating_add(decisions);
                    snapshot.feeds_connected = supervisor.connected_count();
                    snapshot.orders_journaled_total = orders_journaled;
                }).await;
            }
            _ = reconcile_tick.tick() => {
                let mut cycle_safe_hold = BTreeSet::new();
                let mut next_positions = BTreeMap::new();
                let mut next_orders = BTreeMap::new();
                let mut open_orders = 0_usize;
                let mut clean = true;
                for venue in venues.iter().copied() {
                    match execution.reconcile_once(venue).await {
                        Ok(cycle) => {
                            if !cycle.report.clean() {
                                clean = false;
                            }
                            cycle_safe_hold.extend(cycle.report.safe_hold_assets.iter().cloned());
                            for order in cycle.orders {
                                if !order.is_terminal() {
                                    open_orders = open_orders.saturating_add(1);
                                }
                                next_orders.insert(order.client_order_id.clone(), order);
                            }
                            for position in cycle.positions {
                                next_positions.insert(position.key(), position);
                            }
                        }
                        Err(error) => {
                            clean = false;
                            tracing::error!(
                                ?venue,
                                %error,
                                "continuous reconcile failed; freezing new exposure"
                            );
                        }
                    }
                }
                if clean {
                    latest_positions = next_positions;
                    latest_orders = next_orders;
                    reconcile_safe_hold = cycle_safe_hold;
                    runtime_pins = derive_runtime_pins(
                        &latest_positions,
                        &latest_orders,
                        &reconcile_safe_hold,
                    );
                    checklist.pass(StartupGate::OwnershipReconciled);
                    checklist.pass(StartupGate::UnknownStateClear);
                } else {
                    runtime_pins.safe_hold.extend(cycle_safe_hold);
                    checklist.fail(
                        StartupGate::OwnershipReconciled,
                        "continuous reconcile is not clean",
                    );
                }
                risk_limits.allow_new_exposure =
                    trading_started && clean && supervisor.all_connected();
                health.mutate(|snapshot| {
                    snapshot.open_orders = open_orders;
                    if !clean {
                        snapshot.ready = false;
                        snapshot.last_error = Some("continuous reconcile entered SAFE_HOLD".into());
                    }
                }).await;
            }
            _ = universe_tick.tick(), if dynamic_enabled => {
                risk_limits.allow_new_exposure = false;
                match refresh_dynamic_universe(
                    &mut registry,
                    &runtime_pins.strategy,
                    &runtime_pins.operational,
                ).await {
                    Ok(next_resolution) => {
                        let next_feeds = registry.subscriptions();
                        let topology_result: Result<()> = async {
                            validate_live_feeds(&next_feeds)?;
                            ensure_venues_registered(&next_feeds, &venues)?;
                            #[cfg(feature = "ibkr-marketdata")]
                            if let Some(dynamic) = ibkr_dynamic.as_mut() {
                                let desired = desired_ibkr_assets(
                                    &next_feeds,
                                    &next_resolution.operational_pins,
                                );
                                dynamic
                                    .sync_assets(&desired, &next_resolution.descriptors)
                                    .await?;
                            }
                            feed_tasks.sync(
                                next_feeds.clone(),
                                &mut supervisor,
                                &event_tx,
                                &fatal_tx,
                            )?;
                            Ok(())
                        }.await;
                        match topology_result {
                            Ok(()) => {
                                feeds = next_feeds;
                                dynamic_resolution = next_resolution;
                                health.mutate(|snapshot| {
                                    snapshot.feeds_total = feeds.len();
                                }).await;
                                tracing::info!(
                                    strategies = registry.len(),
                                    feeds = feeds.len(),
                                    strategy_assets = dynamic_resolution.strategy_assets.len(),
                                    operational_pins = dynamic_resolution.operational_pins.len(),
                                    "dynamic universe refresh applied"
                                );
                            }
                            Err(error) => {
                                tracing::error!(%error, "dynamic topology apply failed; exposure remains frozen");
                                health.mutate(|snapshot| {
                                    snapshot.ready = false;
                                    snapshot.last_error = Some(error.to_string());
                                }).await;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "dynamic universe refresh failed; exposure remains frozen");
                        health.mutate(|snapshot| {
                            snapshot.ready = false;
                            snapshot.last_error = Some(error.to_string());
                        }).await;
                    }
                }
            }
            maybe_fatal = fatal_rx.recv() => {
                let Some(fatal) = maybe_fatal else {
                    continue;
                };
                supervisor.mark_failed(&fatal.spec);
                risk_limits.allow_new_exposure = false;
                let reason = fatal.reason.clone();
                tracing::error!(
                    venue = ?fatal.spec.venue,
                    asset = %fatal.spec.asset,
                    %reason,
                    "market-data feed exhausted reconnect budget; restarting fail-closed"
                );
                if let Err(error) = feed_tasks.restart_failed(
                    fatal.spec,
                    &mut supervisor,
                    &event_tx,
                    &fatal_tx,
                ) {
                    tracing::error!(%error, "failed to restart exhausted market-data feed");
                }
                health.mutate(|snapshot| {
                    snapshot.ready = false;
                    snapshot.last_error = Some(reason);
                    snapshot.feeds_connected = supervisor.connected_count();
                }).await;
            }
            changed = lease_health.changed() => {
                if changed.is_err() {
                    break Err(anyhow::anyhow!("lease heartbeat channel closed"));
                }
                match lease_health.borrow().clone() {
                    LeaseHealth::Healthy => {}
                    LeaseHealth::Lost(reason) => {
                        risk_limits.allow_new_exposure = false;
                        health.mutate(|snapshot| {
                            snapshot.ready = false;
                            snapshot.lease_healthy = false;
                            snapshot.last_error = Some(reason.clone());
                        }).await;
                        break Err(anyhow::anyhow!("runtime lease lost: {reason}"));
                    }
                    LeaseHealth::Stopped => {
                        break Err(anyhow::anyhow!(
                            "runtime lease heartbeat stopped unexpectedly"
                        ));
                    }
                }
            }
            _ = health_tick.tick() => {
                supervisor.refresh_staleness(now_ns(), max_staleness_ns);
                if supervisor.all_connected() {
                    checklist.pass(StartupGate::MarketDataSynchronized);
                } else {
                    checklist.fail(
                        StartupGate::MarketDataSynchronized,
                        format!(
                            "{}/{} derived feeds connected",
                            supervisor.connected_count(),
                            supervisor.feed_count()
                        ),
                    );
                    risk_limits.allow_new_exposure = false;
                }
                let ready = checklist.ready_for(config.mode);
                let blocking = blocking_gate_names(&checklist, config.mode);
                health.mutate(|snapshot| {
                    snapshot.ready = ready;
                    snapshot.feeds_connected = supervisor.connected_count();
                    snapshot.blocking_gates = blocking;
                    if ready {
                        snapshot.last_error = None;
                    }
                }).await;
            }
            Some(command) = control_rx.recv() => {
                match command {
                    ControlCommand::ReloadStrategies => {
                        match reload_strategies(&mut registry, strategy_dir.as_deref()) {
                            Ok(ids) => {
                                risk_limits.allow_new_exposure = false;
                                tracing::info!(strategies = ?ids, "strategy definitions reloaded; waiting for topology refresh");
                            }
                            Err(error) => {
                                let reason = error.to_string();
                                tracing::warn!(%reason, "strategy reload rejected");
                                health.mutate(|snapshot| snapshot.last_error = Some(reason)).await;
                            }
                        }
                    }
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                tracing::info!("shutdown signal received");
                break Ok(());
            }
        }
    };

    trading_started = false;
    risk_limits.allow_new_exposure = false;
    feed_tasks.stop_all();
    if let Err(error) = apply_shutdown_policy(
        config.shutdown_policy,
        execution.as_ref(),
        store.as_ref(),
        &venues,
    )
    .await
    {
        tracing::warn!(%error, "shutdown policy could not be completed cleanly");
    }
    let _ = trading_started;
    health.mutate(|snapshot| snapshot.ready = false).await;
    heartbeat.stop().await;
    if let Err(error) = store.release_lease(&lease).await {
        tracing::warn!(%error, "failed to release runtime lease during shutdown");
    }
    result
}

fn derive_runtime_pins(
    positions: &BTreeMap<AssetKey, VenuePosition>,
    orders: &BTreeMap<String, OrderRecord>,
    reconcile_safe_hold: &BTreeSet<AssetKey>,
) -> RuntimePins {
    let mut pins = RuntimePins {
        safe_hold: reconcile_safe_hold.clone(),
        ..RuntimePins::default()
    };
    for (key, position) in positions {
        if position.quantity.is_zero() {
            continue;
        }
        pins.operational.insert(key.clone());
        match &position.ownership {
            Ownership::Strategy(_) => {
                pins.strategy.insert(key.clone());
                pins.strategy_positions.insert(key.clone(), position.quantity);
            }
            Ownership::Manual | Ownership::Unknown => {
                // Never allow automation to merge into a position that emergency
                // exit is forbidden to touch.
                pins.safe_hold.insert(key.clone());
            }
        }
    }
    for order in orders.values().filter(|order| !order.is_terminal()) {
        let key = AssetKey::new(order.venue, order.asset.clone());
        pins.operational.insert(key.clone());
        pins.strategy.insert(key.clone());
        if matches!(order.state, pg_oms::OrderState::Unknown | pg_oms::OrderState::PendingSubmit) {
            pins.safe_hold.insert(key);
        }
    }
    pins
}

fn configured_live_venues(
    registry: &StrategyRegistry,
    feeds: &[FeedSpec],
    operational_pins: &BTreeSet<AssetKey>,
) -> BTreeSet<Venue> {
    let mut venues = feeds.iter().map(|feed| feed.venue).collect::<BTreeSet<_>>();
    venues.extend(operational_pins.iter().map(|key| key.venue));
    for template in registry.dynamic_templates() {
        venues.extend(
            template
                .definition
                .universe
                .dynamic_sources()
                .iter()
                .map(|source| source.venue),
        );
    }
    venues
}

async fn build_real_adapter_registry(
    feeds: &[FeedSpec],
    operational_pins: &BTreeSet<AssetKey>,
    descriptors: &BTreeMap<AssetKey, InstrumentDescriptor>,
    venues: &BTreeSet<Venue>,
    config: &RunConfig,
) -> Result<BuiltAdapters> {
    let mut registry = AdapterRegistry::default();

    if venues.contains(&Venue::Hyperliquid) {
        #[cfg(feature = "hyperliquid-marketdata")]
        {
            let network = hyperliquid_network()?;
            if config.mode == RunMode::Paper && network != HyperliquidNetwork::Testnet {
                bail!("Hyperliquid PG_RUN_MODE=paper requires HYPERLIQUID_NETWORK=testnet");
            }
            let account_address = required_value("HYPERLIQUID_ACCOUNT_ADDRESS")?;
            let private_key = match optional_secret("HYPERLIQUID_AGENT_PRIVATE_KEY")? {
                Some(value) => value,
                None => required_secret("HYPERLIQUID_PRIVATE_KEY")?,
            };
            let max_market_slippage = env::var("HYPERLIQUID_MAX_MARKET_SLIPPAGE")
                .unwrap_or_else(|_| "0.005".into())
                .parse::<f64>()
                .context("invalid HYPERLIQUID_MAX_MARKET_SLIPPAGE")?;
            let adapter = HyperliquidExecutionAdapter::connect_with_private_key(
                HyperliquidExecutionConfig {
                    network,
                    account_address,
                    max_market_slippage,
                },
                &private_key,
            )
            .await
            .context("failed to initialize Hyperliquid real execution adapter")?;
            adapter.positions().await.context("Hyperliquid positions read failed")?;
            adapter.open_orders().await.context("Hyperliquid open-orders read failed")?;
            adapter.account_snapshot().await.context("Hyperliquid account snapshot failed")?;
            registry.register(Venue::Hyperliquid, Arc::new(adapter));
        }
        #[cfg(not(feature = "hyperliquid-marketdata"))]
        bail!("Hyperliquid strategy enabled but pg-core was built without Hyperliquid SDK support");
    }

    #[cfg(feature = "ibkr-marketdata")]
    let mut ibkr_dynamic = None;
    if venues.contains(&Venue::InteractiveBrokers) {
        #[cfg(feature = "ibkr-marketdata")]
        {
            let allow_software_reduce_only = bool_env("IBKR_ALLOW_SOFTWARE_REDUCE_ONLY", false)?;
            if config.mode == RunMode::Live && !allow_software_reduce_only {
                bail!(
                    "IBKR live execution requires IBKR_ALLOW_SOFTWARE_REDUCE_ONLY=true; ordinary stock orders have no native atomic reduce-only flag"
                );
            }
            let base = ibkr_config_from_env()?;
            let execution_client_base = env::var("IBKR_EXECUTION_CLIENT_ID_BASE")
                .ok()
                .map(|value| {
                    value
                        .parse::<i32>()
                        .context("invalid IBKR_EXECUTION_CLIENT_ID_BASE")
                })
                .transpose()?
                .unwrap_or_else(|| base.client_id.saturating_add(100));
            let composite = Arc::new(CompositeExecutionAdapter::new(Venue::InteractiveBrokers));
            let mut dynamic = DynamicIbkrExecution {
                composite: composite.clone(),
                base,
                next_client_id: execution_client_base,
                allow_software_reduce_only,
            };
            let mut assets = desired_ibkr_assets(feeds, operational_pins);
            if assets.is_empty()
                && let Some(key) = descriptors
                    .keys()
                    .find(|key| key.venue == Venue::InteractiveBrokers)
            {
                assets.insert(key.asset.clone());
            }
            if assets.is_empty() {
                bail!("IBKR venue enabled but discovery produced no account-capable instrument");
            }
            dynamic.sync_assets(&assets, descriptors).await?;
            composite.positions().await.context("IBKR positions read failed")?;
            composite.open_orders().await.context("IBKR open-orders read failed")?;
            composite.account_snapshot().await.context("IBKR account snapshot failed")?;
            registry.register(Venue::InteractiveBrokers, composite);
            ibkr_dynamic = Some(dynamic);
        }
        #[cfg(not(feature = "ibkr-marketdata"))]
        bail!("IBKR strategy enabled but pg-core was built without ibkr-marketdata/SDK support");
    }

    if venues.contains(&Venue::BinancePm) {
        bail!("BINANCE_PM remains fail-closed: no production execution/recovery adapter is registered");
    }

    Ok(BuiltAdapters {
        registry,
        #[cfg(feature = "ibkr-marketdata")]
        ibkr_dynamic,
    })
}

#[cfg(feature = "ibkr-marketdata")]
fn desired_ibkr_assets(feeds: &[FeedSpec], pins: &BTreeSet<AssetKey>) -> BTreeSet<String> {
    feeds
        .iter()
        .filter(|feed| feed.venue == Venue::InteractiveBrokers)
        .map(|feed| feed.asset.clone())
        .chain(
            pins.iter()
                .filter(|key| key.venue == Venue::InteractiveBrokers)
                .map(|key| key.asset.clone()),
        )
        .collect()
}

fn ensure_venues_registered(feeds: &[FeedSpec], venues: &[Venue]) -> Result<()> {
    let missing = feeds
        .iter()
        .map(|feed| feed.venue)
        .filter(|venue| !venues.contains(venue))
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        bail!("universe refresh introduced unregistered live venues: {missing:?}");
    }
    Ok(())
}

async fn submit_intent(
    execution: &DurableExecution<PostgresStore>,
    limits: &RiskLimits,
    safe_hold_assets: &BTreeSet<AssetKey>,
    strategy_id: &str,
    intent: &OrderIntent,
) -> Option<String> {
    let client_order_id = intent.client_order_id();
    if intent.effect == ExposureEffect::Increase
        && safe_hold_assets.contains(&AssetKey::new(intent.venue, intent.asset.clone()))
    {
        tracing::warn!(
            %strategy_id,
            %client_order_id,
            asset = %intent.asset,
            venue = ?intent.venue,
            "SAFE_HOLD blocks new exposure"
        );
        return None;
    }
    if let RiskDecision::Reject { code, reason } = evaluate_order(intent, limits) {
        tracing::warn!(
            %strategy_id,
            %client_order_id,
            %code,
            %reason,
            "order intent rejected by risk gate"
        );
        return None;
    }
    match execution.dispatch(intent).await {
        Ok(record) => {
            tracing::info!(
                %strategy_id,
                %client_order_id,
                venue = ?intent.venue,
                asset = %intent.asset,
                side = ?intent.side,
                quantity = %intent.quantity,
                effect = ?intent.effect,
                state = ?record.state,
                "order journaled and acknowledged by real venue"
            );
            Some(client_order_id)
        }
        Err(error) => {
            tracing::error!(
                %strategy_id,
                %client_order_id,
                %error,
                "real-venue dispatch failed; durable intent retained for reconciliation"
            );
            None
        }
    }
}

async fn strategy_is_busy(
    execution: &DurableExecution<PostgresStore>,
    live: &mut BTreeMap<String, (Venue, String)>,
    strategy_id: &str,
) -> bool {
    let Some((venue, client_order_id)) = live.get(strategy_id).cloned() else {
        return false;
    };
    let Some(adapter) = execution.adapters().get(venue) else {
        return true;
    };
    match adapter.find_order_by_client_id(&client_order_id).await {
        Ok(Some(order))
            if !matches!(
                order.state,
                pg_execution::VenueOrderState::Filled
                    | pg_execution::VenueOrderState::Canceled
                    | pg_execution::VenueOrderState::Rejected
            ) =>
        {
            true
        }
        Ok(_) => {
            live.remove(strategy_id);
            false
        }
        Err(error) => {
            tracing::warn!(
                %strategy_id,
                %client_order_id,
                %error,
                "cannot prove prior order is terminal; keeping strategy busy"
            );
            true
        }
    }
}

fn policy_intent(
    registry: &StrategyRegistry,
    routed: &pg_strategy::registry::RoutedLivePolicyOutput,
    position: &PositionView,
) -> Option<OrderIntent> {
    let policy = registry.policy(&routed.strategy_id)?;
    match (&routed.entry, &routed.exit) {
        (_, ExitPolicyDecision::Matched { rule_id }) if !position.net_quantity.is_zero() => {
            let side = if position.net_quantity > Decimal::ZERO {
                Side::Sell
            } else {
                Side::Buy
            };
            Some(build_intent(
                policy,
                side,
                position.net_quantity.abs(),
                ExposureEffect::ReduceOnly,
                rule_id,
            ))
        }
        (EntryPolicyDecision::Matched { rule_id, side }, _) if position.net_quantity.is_zero() => {
            if *side == Side::Sell && !policy.allow_short {
                return None;
            }
            Some(build_intent(
                policy,
                *side,
                policy.order_quantity,
                ExposureEffect::Increase,
                rule_id,
            ))
        }
        _ => None,
    }
}

fn build_intent(
    policy: &pg_strategy::definition::PolicyInstance,
    side: Side,
    quantity: Decimal,
    effect: ExposureEffect,
    rule_id: &str,
) -> OrderIntent {
    OrderIntent {
        intent_id: uuid::Uuid::new_v4(),
        strategy_id: policy.strategy_id.clone(),
        asset: policy.instrument.asset.clone(),
        venue: policy.instrument.venue,
        side,
        quantity,
        limit_price: None,
        effect,
        source_signal_id: Some(format!("policy-rule:{rule_id}")),
    }
}

fn position_view(quantity: Decimal) -> PositionView {
    PositionView {
        net_quantity: quantity,
        average_entry_price: None,
        filled_entries: 0,
        unrealized_return: None,
        peak_return: None,
    }
}

fn event_asset_key(event: &MarketEvent) -> AssetKey {
    match event {
        MarketEvent::Trade(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::BestBidAsk(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::L2Book(value) => AssetKey::new(value.venue, value.asset.clone()),
        MarketEvent::Candle(value) => AssetKey::new(value.venue, value.asset.clone()),
    }
}

async fn apply_shutdown_policy(
    policy: ShutdownPolicy,
    execution: &DurableExecution<PostgresStore>,
    store: &PostgresStore,
    venues: &[Venue],
) -> Result<()> {
    if policy == ShutdownPolicy::Preserve {
        return Ok(());
    }

    for venue in venues.iter().copied() {
        for order in store
            .load_orders_for_venue(venue)
            .await?
            .into_iter()
            .filter(|order| !order.is_terminal())
        {
            if let Err(error) = execution.cancel_order(&order).await {
                tracing::warn!(
                    ?venue,
                    client_order_id = %order.client_order_id,
                    %error,
                    "shutdown durable cancel failed"
                );
            }
        }
    }

    if policy != ShutdownPolicy::FlattenOwned {
        return Ok(());
    }

    for venue in venues.iter().copied() {
        let cycle = execution.reconcile_once(venue).await?;
        for position in cycle.positions {
            let Ownership::Strategy(strategy_id) = position.ownership else {
                continue;
            };
            if position.quantity.is_zero() {
                continue;
            }
            let side = if position.quantity > Decimal::ZERO {
                Side::Sell
            } else {
                Side::Buy
            };
            let intent = OrderIntent {
                intent_id: uuid::Uuid::new_v4(),
                strategy_id,
                asset: position.asset,
                venue,
                side,
                quantity: position.quantity.abs(),
                limit_price: None,
                effect: ExposureEffect::ReduceOnly,
                source_signal_id: Some("shutdown:flatten_owned".into()),
            };
            let limits = RiskLimits {
                allow_new_exposure: false,
                ..RiskLimits::default()
            };
            let _ = submit_intent(
                execution,
                &limits,
                &BTreeSet::new(),
                "shutdown:flatten_owned",
                &intent,
            )
            .await;
        }
    }
    Ok(())
}

fn reload_strategies(
    registry: &mut StrategyRegistry,
    strategy_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let dir = strategy_dir.context("no strategy directory is configured for reload")?;
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("toml"))
        .collect::<Vec<_>>();
    entries.sort();
    if entries.is_empty() {
        bail!("no strategy definitions found in {}", dir.display());
    }
    let mut reloaded = Vec::new();
    for entry in entries {
        reloaded.extend(
            registry
                .reload_file(&entry)
                .with_context(|| format!("reload rejected for {}", entry.display()))?,
        );
    }
    Ok(reloaded)
}

fn blocking_gate_names(checklist: &StartupChecklist, mode: RunMode) -> Vec<String> {
    checklist
        .blocking_gates(mode)
        .into_iter()
        .map(|(gate, status)| format!("{gate:?}: {status:?}"))
        .collect()
}

fn supported_live_venues() -> Vec<Venue> {
    #[allow(unused_mut)]
    let mut venues = Vec::new();
    #[cfg(feature = "hyperliquid-marketdata")]
    venues.push(Venue::Hyperliquid);
    #[cfg(feature = "ibkr-marketdata")]
    venues.push(Venue::InteractiveBrokers);
    venues
}

fn validate_live_feeds(feeds: &[FeedSpec]) -> Result<()> {
    let supported = supported_live_venues();
    let unsupported = feeds
        .iter()
        .filter(|feed| !supported.contains(&feed.venue))
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        bail!(
            "no real runtime venue is compiled for feeds {unsupported:?}; available: {supported:?}"
        );
    }
    Ok(())
}

enum FeedEndpoint {
    #[cfg(feature = "hyperliquid-marketdata")]
    Hyperliquid(HyperliquidNetwork),
    #[cfg(feature = "ibkr-marketdata")]
    Ibkr {
        config: IbkrConfig,
        instrument: IbkrStockSpec,
    },
}

fn feed_endpoint(sequence: usize, spec: &FeedSpec) -> Result<FeedEndpoint> {
    match spec.venue {
        #[cfg(feature = "hyperliquid-marketdata")]
        Venue::Hyperliquid => Ok(FeedEndpoint::Hyperliquid(hyperliquid_network()?)),
        #[cfg(feature = "ibkr-marketdata")]
        Venue::InteractiveBrokers => {
            let mut config = ibkr_config_from_env()?;
            let market_data_base = env::var("IBKR_MARKETDATA_CLIENT_ID_BASE")
                .ok()
                .map(|value| {
                    value
                        .parse::<i32>()
                        .context("invalid IBKR_MARKETDATA_CLIENT_ID_BASE")
                })
                .transpose()?
                .unwrap_or_else(|| config.client_id.saturating_add(1_000));
            config.client_id = market_data_base
                .checked_add(i32::try_from(sequence).context("too many IBKR feed generations")?)
                .context("IBKR market-data client id overflow")?;
            Ok(FeedEndpoint::Ibkr {
                config,
                instrument: ibkr_stock_spec(&spec.asset),
            })
        }
        other => bail!("no real market-data endpoint for venue {other:?}"),
    }
}

async fn open_feed_stream(
    endpoint: &FeedEndpoint,
    spec: &FeedSpec,
    sink: &mpsc::Sender<MarketEvent>,
) -> Result<(), pg_marketdata::MarketDataError> {
    match endpoint {
        #[cfg(feature = "hyperliquid-marketdata")]
        FeedEndpoint::Hyperliquid(network) => {
            let mut source = HyperliquidMarketDataSource::connect(*network).await?;
            source.stream(spec.clone(), sink.clone()).await
        }
        #[cfg(feature = "ibkr-marketdata")]
        FeedEndpoint::Ibkr { config, instrument } => {
            let mut source = IbkrMarketDataSource::connect(config, instrument.clone()).await?;
            source.stream(spec.clone(), sink.clone()).await
        }
        #[allow(unreachable_patterns)]
        _ => Err(pg_marketdata::MarketDataError::Subscription(
            "no compiled live market-data source".into(),
        )),
    }
}

fn spawn_live_feed(
    sequence: usize,
    spec: FeedSpec,
    sink: mpsc::Sender<MarketEvent>,
    fatal: mpsc::Sender<FeedFatal>,
) -> Result<JoinHandle<()>> {
    let endpoint = feed_endpoint(sequence, &spec)?;
    let max_reconnects = env::var("PG_MARKET_MAX_RECONNECTS")
        .unwrap_or_else(|_| "50".into())
        .parse::<u32>()
        .context("invalid PG_MARKET_MAX_RECONNECTS")?;
    if max_reconnects == 0 {
        bail!("PG_MARKET_MAX_RECONNECTS must be positive");
    }
    Ok(tokio::spawn(async move {
        let mut failures = 0_u32;
        let mut backoff = Duration::from_secs(1);
        loop {
            let outcome = open_feed_stream(&endpoint, &spec, &sink).await;
            failures = failures.saturating_add(1);
            let reason = outcome
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "market-data stream ended".into());
            tracing::warn!(
                venue = ?spec.venue,
                asset = %spec.asset,
                kind = ?spec.kind,
                failures,
                %reason,
                "market-data subscription reconnecting"
            );
            if failures >= max_reconnects {
                let _ = fatal
                    .send(FeedFatal {
                        spec: spec.clone(),
                        reason,
                    })
                    .await;
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }))
}

fn feed_key(spec: &FeedSpec) -> String {
    format!("{:?}:{}:{:?}", spec.venue, spec.asset, spec.kind)
}

#[cfg(feature = "ibkr-marketdata")]
fn ibkr_config_from_env() -> Result<IbkrConfig> {
    Ok(IbkrConfig {
        gateway_addr: env::var("IBKR_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:4002".into()),
        client_id: env::var("IBKR_CLIENT_ID")
            .unwrap_or_else(|_| "17".into())
            .parse()
            .context("invalid IBKR_CLIENT_ID")?,
        account: env::var("IBKR_ACCOUNT")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        market_depth_rows: env::var("IBKR_MARKET_DEPTH_ROWS")
            .unwrap_or_else(|_| "5".into())
            .parse()
            .context("invalid IBKR_MARKET_DEPTH_ROWS")?,
    })
}

#[cfg(feature = "ibkr-marketdata")]
fn ibkr_stock_spec(asset: &str) -> IbkrStockSpec {
    let mut spec = IbkrStockSpec::smart_us(asset);
    if let Ok(exchange) = env::var("IBKR_DEFAULT_EXCHANGE")
        && !exchange.trim().is_empty()
    {
        spec.exchange = exchange;
    }
    if let Ok(currency) = env::var("IBKR_DEFAULT_CURRENCY")
        && !currency.trim().is_empty()
    {
        spec.currency = currency;
    }
    spec
}

#[cfg(feature = "ibkr-marketdata")]
fn ibkr_stock_spec_with_descriptor(
    asset: &str,
    descriptors: &BTreeMap<AssetKey, InstrumentDescriptor>,
) -> IbkrStockSpec {
    let mut spec = ibkr_stock_spec(asset);
    let key = AssetKey::new(Venue::InteractiveBrokers, asset.to_owned());
    let Some(descriptor) = descriptors.get(&key) else {
        return spec;
    };
    if let Some(exchange) = descriptor.exchange.as_deref()
        && !exchange.trim().is_empty()
    {
        spec.exchange = exchange.to_owned();
    }
    if let Some(primary) = descriptor.primary_exchange.as_deref()
        && !primary.trim().is_empty()
    {
        spec.primary_exchange = Some(primary.to_owned());
    }
    if let Some(currency) = descriptor.currency.as_deref()
        && !currency.trim().is_empty()
    {
        spec.currency = currency.to_owned();
    }
    spec.con_id = descriptor
        .venue_instrument_id
        .as_deref()
        .and_then(|value| value.parse::<i32>().ok());
    spec
}

#[cfg(feature = "hyperliquid-marketdata")]
fn hyperliquid_network() -> Result<HyperliquidNetwork> {
    match env::var("PG_HYPERLIQUID_NETWORK")
        .or_else(|_| env::var("HYPERLIQUID_NETWORK"))
        .unwrap_or_else(|_| "mainnet".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "mainnet" => Ok(HyperliquidNetwork::Mainnet),
        "testnet" => Ok(HyperliquidNetwork::Testnet),
        other => bail!("invalid Hyperliquid network {other}; expected mainnet or testnet"),
    }
}

fn env_u64(name: &'static str, default: u64) -> Result<u64> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("invalid {name}"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos() as u64
}
