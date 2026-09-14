use crate::health::{ControlCommand, HealthSnapshot, HealthState};
use anyhow::{Context, Result, bail};
use pg_execution::{
    ExecutionAdapter, OrderLocator, ShadowAdapterConfig, ShadowExecutionAdapter, ShadowFillMode,
    ShadowPosition,
};
use pg_marketdata::{FeedSpec, MarketDataSource, MarketEvent, SubscriptionSupervisor};
use pg_orchestrator::{AdapterRegistry, DurableExecution};
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
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::json;
use std::{collections::BTreeMap, env, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::MissedTickBehavior};

#[cfg(feature = "hyperliquid-marketdata")]
use pg_hyperliquid::{HyperliquidMarketDataSource, HyperliquidNetwork};
#[cfg(feature = "ibkr-marketdata")]
use pg_ibkr::{IbkrConfig, IbkrMarketDataSource, IbkrStockSpec};

#[derive(Debug)]
struct FeedFatal {
    spec: FeedSpec,
    reason: String,
}

pub async fn serve(config: RunConfig, mut registry: StrategyRegistry) -> Result<()> {
    if config.mode != RunMode::Shadow {
        bail!(
            "pg-core --serve currently accepts PG_RUN_MODE=shadow only; paper/live remain fail-closed until full execution/reconcile control-plane wiring is complete"
        );
    }
    // Shadow mode is only safe because every registered execution adapter is the
    // in-process simulated venue. Refuse to combine it with real-venue routing.
    if config.routes_to_real_venue() {
        bail!("refusing to start the shadow daemon with real-venue routing enabled");
    }
    if registry.is_empty() {
        bail!("shadow daemon requires at least one enabled strategy definition");
    }
    let strategy_dir = crate::resolve_strategy_dir();

    let database_url = env::var("PG_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .context("PG_DATABASE_URL (or DATABASE_URL) is required for --serve")?;
    let store = Arc::new(PostgresStore::connect(&database_url, 8).await?);
    store.migrate().await?;
    store.ping().await?;

    let mut checklist = StartupChecklist::new();
    checklist.pass(StartupGate::DatabaseReachable);

    let lease_key = env::var("PG_LEASE_KEY")
        .unwrap_or_else(|_| format!("pg-core:{}:shadow", config.environment));
    let lease = store
        .acquire_lease(
            &lease_key,
            &config.instance_id,
            config.lease_ttl_seconds as i32,
        )
        .await?;
    checklist.pass(StartupGate::RuntimeLeaseAcquired);

    // Every strategy-originated order now travels
    //
    //   strategy decision -> pg-risk -> OMS -> journal -> execution adapter
    //
    // In shadow that adapter is an in-process simulated venue, so the durable path
    // is genuinely exercised while the counterparty stays local. Nothing here can
    // reach a real exchange: the Hyperliquid execution adapter is never constructed
    // and live/paper modes are rejected at the top of this function.
    let shadow_fill_mode = shadow_fill_mode_from_env()?;
    let (adapters, shadow_venues) = shadow_adapter_registry(shadow_fill_mode);
    let execution = Arc::new(DurableExecution::new(
        store.clone(),
        lease.clone(),
        adapters,
    ));
    // Exposure created here is simulated. The real-venue equivalent stays closed:
    // `serve` refuses to run whenever real-venue routing is enabled.
    let risk_limits = RiskLimits {
        allow_new_exposure: true,
        ..RiskLimits::default()
    };

    store
        .append_event(
            &format!("runtime:{}", config.instance_id),
            "runtime.boot",
            &json!({
                "environment": config.environment,
                "instance_id": config.instance_id,
                "mode": "shadow",
                "fencing_token": lease.fencing_token,
            }),
            lease.fencing_token,
        )
        .await?;
    checklist.pass(StartupGate::JournalWritable);
    checklist.pass(StartupGate::StrategyAllowlistLoaded);

    let feeds = registry.subscriptions();
    if feeds.is_empty() {
        bail!("enabled strategies derived zero market-data subscriptions");
    }
    validate_shadow_feeds(&feeds)?;

    apply_execution_gates(
        &mut checklist,
        &config,
        store.as_ref(),
        &execution,
        &shadow_venues,
        &feeds,
    )
    .await?;

    let health_addr: SocketAddr = env::var("PG_HEALTH_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .context("invalid PG_HEALTH_ADDR")?;
    let health = HealthState::new(HealthSnapshot::booting("shadow"));
    health
        .mutate(|snapshot| {
            snapshot.lease_healthy = true;
            snapshot.feeds_total = feeds.len();
            snapshot.blocking_gates = blocking_gate_names(&checklist);
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
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<FeedFatal>(feeds.len().max(1));

    for spec in feeds.iter().cloned() {
        spawn_shadow_feed(spec, event_tx.clone(), fatal_tx.clone())?;
    }
    drop(fatal_tx);

    let mut supervisor = SubscriptionSupervisor::new();
    supervisor.apply_derived_feeds(feeds);
    let max_staleness_ns = config.max_market_staleness_ms.saturating_mul(1_000_000);
    let mut health_tick = tokio::time::interval(Duration::from_secs(1));
    health_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!(
        feeds = supervisor.feed_count(),
        strategies = registry.len(),
        policies = registry.policy_count(),
        fencing_token = lease.fencing_token,
        "shadow production daemon started"
    );

    let mut peak_returns: BTreeMap<AssetKey, f64> = BTreeMap::new();
    // Strategy id -> client order id of the intent the policy path is still working.
    // Without this the portable path would re-emit the same entry on every tick.
    let mut live_policy_intents: BTreeMap<String, (Venue, String)> = BTreeMap::new();
    let mut orders_journaled: u64 = 0;
    let result: Result<()> = loop {
        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(anyhow::anyhow!("all market-data event senders stopped"));
                };
                supervisor.observe(&event);

                // Mark the simulated venue before strategies run so a fill priced on
                // this tick and the position view the strategies see agree.
                let position = observe_position(&shadow_venues, &event, &mut peak_returns);

                let automation_outputs = registry.route_event(&event);
                let mut decisions = 0_u64;
                for routed in automation_outputs {
                    let strategy_id = routed.strategy_id.clone();
                    match routed.output.decision {
                        StrategyDecision::Submit(intent) => {
                            // A definition that declares a portable rule graph is owned
                            // by the policy engine. Letting the legacy score machine
                            // dispatch as well would put two independent decision engines
                            // on one instrument and could open the same exposure twice.
                            if registry
                                .policy(&strategy_id)
                                .is_some_and(PolicyInstance::is_policy_driven)
                            {
                                tracing::debug!(
                                    strategy_id = %strategy_id,
                                    "automation intent suppressed: definition is policy-driven"
                                );
                                continue;
                            }
                            decisions = decisions.saturating_add(1);
                            if submit_intent(&execution, &risk_limits, &strategy_id, &intent)
                                .await
                                .is_some()
                            {
                                orders_journaled = orders_journaled.saturating_add(1);
                            }
                        }
                        StrategyDecision::Hold(reason) => {
                            tracing::debug!(strategy_id = %strategy_id, %reason, "strategy hold");
                        }
                        StrategyDecision::Noop => {}
                    }
                }

                // The portable policy path evaluates rule graphs rather than emitting
                // order intents, so translate its decisions here and send them through
                // exactly the same risk / OMS / journal / execution path.
                for routed in registry.route_live_policy_event(&event, &position) {
                    decisions = decisions.saturating_add(1);
                    if strategy_is_busy(&mut live_policy_intents, &shadow_venues, &routed.strategy_id) {
                        continue;
                    }
                    let Some(intent) = policy_intent(&registry, &routed, &position) else {
                        continue;
                    };
                    let strategy_id = routed.strategy_id.clone();
                    if let Some(client_order_id) =
                        submit_intent(&execution, &risk_limits, &strategy_id, &intent).await
                    {
                        orders_journaled = orders_journaled.saturating_add(1);
                        live_policy_intents.insert(strategy_id, (intent.venue, client_order_id));
                    }
                }

                let open_orders = shadow_venues
                    .values()
                    .map(ShadowExecutionAdapter::resting_order_count)
                    .sum::<usize>();
                health.mutate(|snapshot| {
                    snapshot.events_total = snapshot.events_total.saturating_add(1);
                    snapshot.policy_decisions_total = snapshot.policy_decisions_total.saturating_add(decisions);
                    snapshot.feeds_connected = supervisor.connected_count();
                    snapshot.open_orders = open_orders;
                    snapshot.orders_journaled_total = orders_journaled;
                }).await;
            }
            maybe_fatal = fatal_rx.recv() => {
                let Some(fatal) = maybe_fatal else {
                    continue;
                };
                supervisor.mark_failed(&fatal.spec);
                health.mutate(|snapshot| {
                    snapshot.ready = false;
                    snapshot.process_healthy = false;
                    snapshot.last_error = Some(fatal.reason.clone());
                    snapshot.feeds_connected = supervisor.connected_count();
                }).await;
                break Err(anyhow::anyhow!("market-data feed {:?} failed permanently: {}", fatal.spec, fatal.reason));
            }
            changed = lease_health.changed() => {
                if changed.is_err() {
                    health.mutate(|snapshot| {
                        snapshot.ready = false;
                        snapshot.lease_healthy = false;
                        snapshot.last_error = Some("lease heartbeat channel closed".into());
                    }).await;
                    break Err(anyhow::anyhow!("lease heartbeat channel closed"));
                }
                match lease_health.borrow().clone() {
                    LeaseHealth::Healthy => {}
                    LeaseHealth::Lost(reason) => {
                        health.mutate(|snapshot| {
                            snapshot.ready = false;
                            snapshot.lease_healthy = false;
                            snapshot.last_error = Some(reason.clone());
                        }).await;
                        break Err(anyhow::anyhow!("runtime lease lost: {reason}"));
                    }
                    LeaseHealth::Stopped => {
                        break Err(anyhow::anyhow!("runtime lease heartbeat stopped unexpectedly"));
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
                        format!("{}/{} derived feeds connected", supervisor.connected_count(), supervisor.feed_count()),
                    );
                }
                let ready = checklist.ready_for(RunMode::Shadow);
                // Recomputed every tick so /healthz reports which gate is actually
                // holding readiness back, not a snapshot taken at boot.
                let blocking_gates = blocking_gate_names(&checklist);
                health.mutate(|snapshot| {
                    snapshot.ready = ready;
                    snapshot.feeds_connected = supervisor.connected_count();
                    snapshot.blocking_gates = blocking_gates;
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
                                tracing::info!(strategies = ?ids, "strategy definitions reloaded");
                                health.mutate(|snapshot| { snapshot.last_error = None; }).await;
                            }
                            Err(error) => {
                                // A rejected reload is not fatal: the running set is
                                // unchanged because validation happens before the swap.
                                let reason = error.to_string();
                                tracing::warn!(%reason, "strategy reload rejected");
                                health.mutate(|snapshot| { snapshot.last_error = Some(reason); }).await;
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

    // Shutdown policy is an explicit operator choice, never an implicit side effect
    // of the process exiting. It only ever touches strategy-owned exposure.
    if let Err(error) = apply_shutdown_policy(
        config.shutdown_policy,
        &execution,
        &shadow_venues,
        &risk_limits,
    )
    .await
    {
        tracing::warn!(%error, "shutdown policy could not be completed cleanly");
    }

    health
        .mutate(|snapshot| {
            snapshot.ready = false;
        })
        .await;
    heartbeat.stop().await;
    if let Err(error) = store.release_lease(&lease).await {
        tracing::warn!(%error, "failed to release runtime lease during shutdown");
    }
    result
}

/// How the simulated venue treats acknowledged orders.
fn shadow_fill_mode_from_env() -> Result<ShadowFillMode> {
    match env::var("PG_SHADOW_FILL_MODE")
        .unwrap_or_else(|_| "rest".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "rest" => Ok(ShadowFillMode::Rest),
        "immediate" | "immediate_fill" => Ok(ShadowFillMode::ImmediateFill),
        other => bail!("invalid PG_SHADOW_FILL_MODE={other}; expected rest or immediate"),
    }
}

/// Register the in-process venue stand-in for every venue the runtime knows about.
///
/// The returned map keeps typed handles so the daemon can read simulated positions
/// and mark them to market; the registry is what the orchestrator dispatches to.
fn shadow_adapter_registry(
    fill_mode: ShadowFillMode,
) -> (AdapterRegistry, BTreeMap<Venue, ShadowExecutionAdapter>) {
    let mut adapters = AdapterRegistry::default();
    let mut handles = BTreeMap::new();
    for venue in [
        Venue::Hyperliquid,
        Venue::InteractiveBrokers,
        Venue::BinancePm,
    ] {
        let adapter =
            ShadowExecutionAdapter::new(ShadowAdapterConfig::new(venue).with_fill_mode(fill_mode));
        adapters.register(venue, Arc::new(adapter.clone()));
        handles.insert(venue, adapter);
    }
    (adapters, handles)
}

/// Evaluate the read-side startup gates against the registered execution adapters.
///
/// These are the gates the daemon previously never passed, which is why a partially
/// wired runtime could still report itself ready.
async fn apply_execution_gates(
    checklist: &mut StartupChecklist,
    config: &RunConfig,
    store: &PostgresStore,
    execution: &DurableExecution<PostgresStore>,
    shadow_venues: &BTreeMap<Venue, ShadowExecutionAdapter>,
    feeds: &[FeedSpec],
) -> Result<()> {
    let missing = feeds
        .iter()
        .map(|spec| spec.venue)
        .filter(|venue| execution.adapters().get(*venue).is_none())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        checklist.pass(StartupGate::VenueAuthenticated);
    } else {
        checklist.fail(
            StartupGate::VenueAuthenticated,
            format!("no execution adapter registered for {missing:?}"),
        );
    }

    let mut unknown_orders: Vec<String> = Vec::new();
    for (venue, adapter) in shadow_venues {
        let orders = adapter
            .open_orders()
            .await
            .with_context(|| format!("failed to load open orders for {venue:?}"))?;
        let positions = adapter
            .positions()
            .await
            .with_context(|| format!("failed to load positions for {venue:?}"))?;
        let persisted = store.load_orders_for_venue(*venue).await?;
        checklist.pass(StartupGate::OpenOrdersLoaded);
        checklist.pass(StartupGate::PositionsLoaded);
        // A simulated venue only ever holds exposure this runtime journaled itself,
        // so ownership is unambiguous by construction.
        checklist.pass(StartupGate::OwnershipReconciled);
        tracing::debug!(
            venue = ?venue,
            open_orders = orders.len(),
            positions = positions.len(),
            persisted_orders = persisted.len(),
            "execution startup snapshot loaded"
        );
        for record in persisted {
            if record.state == pg_oms::OrderState::Unknown {
                unknown_orders.push(record.client_order_id);
            }
        }
    }
    if unknown_orders.is_empty() {
        checklist.pass(StartupGate::UnknownStateClear);
    } else {
        checklist.fail(
            StartupGate::UnknownStateClear,
            format!("unresolved order state for {unknown_orders:?}"),
        );
    }

    if config.live_trading_enabled {
        checklist.pass(StartupGate::LiveTradingExplicitlyEnabled);
    } else {
        checklist.fail(
            StartupGate::LiveTradingExplicitlyEnabled,
            "PG_LIVE_TRADING is false".to_string(),
        );
    }

    for (gate, status) in checklist.blocking_gates(config.mode) {
        tracing::debug!(gate = ?gate, status = ?status, "startup gate not yet passed");
    }
    Ok(())
}

fn blocking_gate_names(checklist: &StartupChecklist) -> Vec<String> {
    checklist
        .blocking_gates(RunMode::Shadow)
        .into_iter()
        .map(|(gate, status)| format!("{gate:?}: {status:?}"))
        .collect()
}

/// Price and identify the instrument an event belongs to, then mark the simulated
/// venue so fills and unrealized return are computed from the same print.
fn observe_position(
    venues: &BTreeMap<Venue, ShadowExecutionAdapter>,
    event: &MarketEvent,
    peak_returns: &mut BTreeMap<AssetKey, f64>,
) -> PositionView {
    let (venue, asset, mark) = event_mark(event);
    let Some(adapter) = venues.get(&venue) else {
        return PositionView::default();
    };
    if let Some(price) = mark {
        adapter.set_reference_price(price);
    }

    let mut view = position_view(&adapter.position(asset), mark);
    let key = AssetKey::new(venue, asset.to_string());
    if let Some(unrealized) = view.unrealized_return {
        let peak = peak_returns.entry(key).or_insert(unrealized);
        if unrealized > *peak {
            *peak = unrealized;
        }
        view.peak_return = Some(*peak);
    } else {
        peak_returns.remove(&key);
    }
    view
}

fn position_view(position: &ShadowPosition, mark: Option<Decimal>) -> PositionView {
    let unrealized_return = match (position.average_entry_price, mark) {
        (Some(entry), Some(mark)) if entry > Decimal::ZERO => {
            ((mark - entry) / entry).to_f64().map(|raw| {
                if position.net_quantity.is_sign_negative() {
                    -raw
                } else {
                    raw
                }
            })
        }
        _ => None,
    };
    PositionView {
        net_quantity: position.net_quantity,
        average_entry_price: position.average_entry_price,
        filled_entries: position.filled_entries,
        unrealized_return,
        peak_return: None,
    }
}

/// Best available mark for an event: last trade, BBO mid, book mid or candle close.
fn event_mark(event: &MarketEvent) -> (Venue, &str, Option<Decimal>) {
    match event {
        MarketEvent::Trade(trade) => (trade.venue, trade.asset.as_str(), Some(trade.price)),
        MarketEvent::BestBidAsk(bbo) => (
            bbo.venue,
            bbo.asset.as_str(),
            mid_price(bbo.bid_price, bbo.ask_price),
        ),
        MarketEvent::L2Book(book) => {
            let mark = match (
                book.bids.first().map(|level| level.price),
                book.asks.first().map(|level| level.price),
            ) {
                (Some(bid), Some(ask)) => mid_price(bid, ask),
                (Some(bid), None) => Some(bid),
                (None, Some(ask)) => Some(ask),
                (None, None) => None,
            };
            (book.venue, book.asset.as_str(), mark)
        }
        MarketEvent::Candle(candle) => (candle.venue, candle.asset.as_str(), Some(candle.close)),
    }
}

fn mid_price(bid: Decimal, ask: Decimal) -> Option<Decimal> {
    if bid > Decimal::ZERO && ask > Decimal::ZERO {
        Some((bid + ask) / Decimal::from(2))
    } else if bid > Decimal::ZERO {
        Some(bid)
    } else if ask > Decimal::ZERO {
        Some(ask)
    } else {
        None
    }
}

/// Send one strategy intent through risk, OMS, journal and the execution adapter.
///
/// Returns the stable client order id when the intent was journaled and acknowledged.
async fn submit_intent(
    execution: &DurableExecution<PostgresStore>,
    limits: &RiskLimits,
    strategy_id: &str,
    intent: &OrderIntent,
) -> Option<String> {
    let client_order_id = intent.client_order_id();

    // Invariant: a new-exposure order never reaches an adapter without passing risk.
    if let RiskDecision::Reject { code, reason } = evaluate_order(intent, limits) {
        tracing::warn!(
            %strategy_id,
            %client_order_id,
            asset = %intent.asset,
            side = ?intent.side,
            %code,
            %reason,
            "order intent rejected by the risk gate"
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
                "order journaled and acknowledged by the simulated venue (no real order sent)"
            );
            Some(client_order_id)
        }
        Err(error) => {
            // Unknown stays unknown: the durable record is already persisted, so the
            // intent can be reconciled by its client id instead of being replaced.
            tracing::error!(
                %strategy_id,
                %client_order_id,
                %error,
                "order dispatch failed; intent remains journaled for reconciliation"
            );
            None
        }
    }
}

/// Whether the policy path already has a working order for this strategy.
fn strategy_is_busy(
    live: &mut BTreeMap<String, (Venue, String)>,
    venues: &BTreeMap<Venue, ShadowExecutionAdapter>,
    strategy_id: &str,
) -> bool {
    let Some((venue, client_order_id)) = live.get(strategy_id) else {
        return false;
    };
    let still_working = venues
        .get(venue)
        .is_some_and(|adapter| adapter.is_client_order_live(client_order_id));
    if !still_working {
        live.remove(strategy_id);
    }
    still_working
}

/// Turn a portable policy evaluation into an order intent.
///
/// Exits are evaluated first and are always reduce-only. Entries only open exposure
/// from flat, and a short entry additionally requires the strategy to opt in.
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
                tracing::debug!(
                    strategy_id = %routed.strategy_id,
                    %rule_id,
                    "short entry suppressed: strategy is long-only"
                );
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
        // Records which rule produced the intent in the append-only journal.
        source_signal_id: Some(format!("policy-rule:{rule_id}")),
    }
}

/// Re-validate and swap strategy definitions without restarting the process.
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

    // Validate everything before swapping anything, so a broken edit cannot leave
    // the runtime with a partially replaced strategy set.
    let mut reloaded = Vec::new();
    for entry in entries {
        let ids = registry
            .reload_file(&entry)
            .with_context(|| format!("reload rejected for {}", entry.display()))?;
        reloaded.extend(ids);
    }
    Ok(reloaded)
}

/// Apply the configured shutdown policy to strategy-owned exposure only.
async fn apply_shutdown_policy(
    policy: ShutdownPolicy,
    execution: &DurableExecution<PostgresStore>,
    venues: &BTreeMap<Venue, ShadowExecutionAdapter>,
    limits: &RiskLimits,
) -> Result<()> {
    tracing::info!(policy = ?policy, "applying shutdown policy");
    if policy == ShutdownPolicy::Preserve {
        return Ok(());
    }

    // Cancel resting orders first: flattening while a stale resting order could still
    // fill would leave the runtime with exposure it never intended to hold.
    for (venue, adapter) in venues {
        for order in adapter.open_orders().await? {
            let Some(client_order_id) = order.client_order_id.clone() else {
                continue;
            };
            if let Err(error) = adapter
                .cancel(OrderLocator {
                    asset: &order.asset,
                    venue_order_id: Some(&order.venue_order_id),
                    client_order_id: &client_order_id,
                })
                .await
            {
                tracing::warn!(venue = ?venue, %client_order_id, %error, "shutdown cancel failed");
            }
        }
    }

    if policy != ShutdownPolicy::FlattenOwned {
        return Ok(());
    }

    for (venue, adapter) in venues {
        for position in adapter.positions().await? {
            let side = if position.quantity > Decimal::ZERO {
                Side::Sell
            } else {
                Side::Buy
            };
            let intent = OrderIntent {
                intent_id: uuid::Uuid::new_v4(),
                strategy_id: "shutdown:flatten_owned".into(),
                asset: position.asset.clone(),
                venue: *venue,
                side,
                quantity: position.quantity.abs(),
                limit_price: None,
                effect: ExposureEffect::ReduceOnly,
                source_signal_id: None,
            };
            submit_intent(execution, limits, "shutdown:flatten_owned", &intent).await;
        }
    }
    Ok(())
}

/// Runtime market-data sources this build actually wires up.
// Each entry is behind a cargo feature, so the collection cannot be written as a
// single literal without duplicating the whole function per feature combination.
#[allow(clippy::vec_init_then_push)]
fn supported_shadow_venues() -> Vec<Venue> {
    #[allow(unused_mut)]
    let mut venues = Vec::new();
    #[cfg(feature = "hyperliquid-marketdata")]
    venues.push(Venue::Hyperliquid);
    #[cfg(feature = "ibkr-marketdata")]
    venues.push(Venue::InteractiveBrokers);
    venues
}

fn validate_shadow_feeds(feeds: &[FeedSpec]) -> Result<()> {
    let supported = supported_shadow_venues();
    if supported.is_empty() {
        bail!(
            "pg-core was built without any runtime market-data source; enable the hyperliquid-marketdata or ibkr-marketdata cargo feature"
        );
    }
    let unsupported = feeds
        .iter()
        .filter(|spec| !supported.contains(&spec.venue))
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        bail!(
            "no runtime market-data source is registered for these feeds (available: {supported:?}); enable the matching cargo feature or disable the strategy: {unsupported:?}"
        );
    }
    Ok(())
}

/// Everything a feed task needs in order to (re)connect, resolved once at startup
/// so a bad setting fails fast instead of on every retry.
enum FeedEndpoint {
    #[cfg(feature = "hyperliquid-marketdata")]
    Hyperliquid(HyperliquidNetwork),
    #[cfg(feature = "ibkr-marketdata")]
    Ibkr {
        config: IbkrConfig,
        instrument: IbkrStockSpec,
    },
}

fn feed_endpoint(spec: &FeedSpec) -> Result<FeedEndpoint> {
    match spec.venue {
        #[cfg(feature = "hyperliquid-marketdata")]
        Venue::Hyperliquid => Ok(FeedEndpoint::Hyperliquid(hyperliquid_network()?)),
        #[cfg(feature = "ibkr-marketdata")]
        Venue::InteractiveBrokers => Ok(FeedEndpoint::Ibkr {
            config: ibkr_config_from_env()?,
            instrument: ibkr_stock_spec(&spec.asset),
        }),
        other => bail!(
            "no runtime market-data source for venue {other:?}; available: {:?}",
            supported_shadow_venues()
        ),
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
        // Reachable only when this build has no market-data features at all;
        // validate_shadow_feeds already refuses to start in that configuration.
        #[allow(unreachable_patterns)]
        _ => Err(pg_marketdata::MarketDataError::Subscription(format!(
            "no runtime market-data source for venue {:?}",
            spec.venue
        ))),
    }
}

fn spawn_shadow_feed(
    spec: FeedSpec,
    sink: mpsc::Sender<pg_marketdata::MarketEvent>,
    fatal: mpsc::Sender<FeedFatal>,
) -> Result<()> {
    let endpoint = feed_endpoint(&spec)?;
    let max_reconnects = env::var("PG_MARKET_MAX_RECONNECTS")
        .unwrap_or_else(|_| "50".into())
        .parse::<u32>()
        .context("invalid PG_MARKET_MAX_RECONNECTS")?;
    if max_reconnects == 0 {
        bail!("PG_MARKET_MAX_RECONNECTS must be positive");
    }
    tracing::info!(
        venue = ?spec.venue,
        asset = %spec.asset,
        kind = ?spec.kind,
        "market-data subscription starting"
    );

    tokio::spawn(async move {
        let mut failures = 0_u32;
        let mut backoff = Duration::from_secs(1);
        loop {
            let outcome = open_feed_stream(&endpoint, &spec, &sink).await;
            failures = failures.saturating_add(1);
            let reason = outcome
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "market-data stream ended without an error".into());
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
    });
    Ok(())
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

/// Map a normalized asset name onto an IBKR contract.
///
/// A FeedSpec only carries venue + asset, so the mapping is deliberately explicit
/// and deployment-wide: US SMART routing by default, overridable per deployment.
/// Per-asset contract metadata (con_id, primary exchange, non-US listings) is a
/// separate concern from strategy definitions and is not inferred here.
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

#[cfg(feature = "hyperliquid-marketdata")]
fn hyperliquid_network() -> Result<HyperliquidNetwork> {
    match env::var("PG_HYPERLIQUID_NETWORK")
        .unwrap_or_else(|_| "mainnet".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "mainnet" => Ok(HyperliquidNetwork::Mainnet),
        "testnet" => Ok(HyperliquidNetwork::Testnet),
        other => bail!("invalid PG_HYPERLIQUID_NETWORK={other}; expected mainnet or testnet"),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos() as u64
}
