use crate::health::{HealthSnapshot, HealthState};
use anyhow::{Context, Result, bail};
use pg_marketdata::{FeedSpec, MarketDataSource, SubscriptionSupervisor};
use pg_runtime::{RunConfig, RunMode, StartupChecklist, StartupGate};
use pg_store::{LeaseHealth, PostgresStore};
use pg_strategy::{StrategyDecision, policy::PositionView, registry::StrategyRegistry};
use pg_types::Venue;
use serde_json::json;
use std::{env, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::MissedTickBehavior};

#[cfg(feature = "hyperliquid-marketdata")]
use pg_hyperliquid::{HyperliquidMarketDataSource, HyperliquidNetwork};

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
    if registry.is_empty() {
        bail!("shadow daemon requires at least one enabled strategy definition");
    }

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

    let health_addr: SocketAddr = env::var("PG_HEALTH_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .context("invalid PG_HEALTH_ADDR")?;
    let health = HealthState::new(HealthSnapshot::booting("shadow"));
    health
        .mutate(|snapshot| {
            snapshot.lease_healthy = true;
            snapshot.feeds_total = feeds.len();
        })
        .await;
    tokio::spawn(crate::health::serve(health_addr, health.clone()));

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

    let position = PositionView::default();
    let result: Result<()> = loop {
        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(anyhow::anyhow!("all market-data event senders stopped"));
                };
                supervisor.observe(&event);

                let automation_outputs = registry.route_event(&event);
                let portable_outputs = registry.route_live_policy_event(&event, &position);
                let mut decisions = 0_u64;
                for routed in automation_outputs {
                    match routed.output.decision {
                        StrategyDecision::Submit(intent) => {
                            decisions = decisions.saturating_add(1);
                            tracing::info!(
                                strategy_id = %routed.strategy_id,
                                client_order_id = %intent.client_order_id(),
                                venue = ?intent.venue,
                                asset = %intent.asset,
                                side = ?intent.side,
                                quantity = %intent.quantity,
                                "shadow order intent (not dispatched)"
                            );
                        }
                        StrategyDecision::Hold(reason) => {
                            tracing::debug!(strategy_id = %routed.strategy_id, %reason, "strategy hold");
                        }
                        StrategyDecision::Noop => {}
                    }
                }
                decisions = decisions.saturating_add(portable_outputs.len() as u64);

                health.mutate(|snapshot| {
                    snapshot.events_total = snapshot.events_total.saturating_add(1);
                    snapshot.policy_decisions_total = snapshot.policy_decisions_total.saturating_add(decisions);
                    snapshot.feeds_connected = supervisor.connected_count();
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
                health.mutate(|snapshot| {
                    snapshot.ready = ready;
                    snapshot.feeds_connected = supervisor.connected_count();
                    if ready {
                        snapshot.last_error = None;
                    }
                }).await;
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                tracing::info!("shutdown signal received");
                break Ok(());
            }
        }
    };

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

fn validate_shadow_feeds(feeds: &[FeedSpec]) -> Result<()> {
    #[cfg(feature = "hyperliquid-marketdata")]
    {
        let unsupported = feeds
            .iter()
            .filter(|spec| spec.venue != Venue::Hyperliquid)
            .collect::<Vec<_>>();
        if !unsupported.is_empty() {
            bail!(
                "default shadow daemon currently wires Hyperliquid public market data only; disable non-Hyperliquid example strategies or add the corresponding runtime source: {unsupported:?}"
            );
        }
        Ok(())
    }
    #[cfg(not(feature = "hyperliquid-marketdata"))]
    {
        let _ = feeds;
        bail!("pg-core was built without hyperliquid-marketdata support")
    }
}

fn spawn_shadow_feed(
    spec: FeedSpec,
    sink: mpsc::Sender<pg_marketdata::MarketEvent>,
    fatal: mpsc::Sender<FeedFatal>,
) -> Result<()> {
    #[cfg(feature = "hyperliquid-marketdata")]
    {
        if spec.venue != Venue::Hyperliquid {
            bail!("unsupported shadow feed venue: {:?}", spec.venue);
        }
        let network = hyperliquid_network()?;
        let max_reconnects = env::var("PG_MARKET_MAX_RECONNECTS")
            .unwrap_or_else(|_| "50".into())
            .parse::<u32>()
            .context("invalid PG_MARKET_MAX_RECONNECTS")?;
        if max_reconnects == 0 {
            bail!("PG_MARKET_MAX_RECONNECTS must be positive");
        }

        tokio::spawn(async move {
            let mut failures = 0_u32;
            let mut backoff = Duration::from_secs(1);
            loop {
                let outcome = match HyperliquidMarketDataSource::connect(network).await {
                    Ok(mut source) => source.stream(spec.clone(), sink.clone()).await,
                    Err(error) => Err(error),
                };
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
    #[cfg(not(feature = "hyperliquid-marketdata"))]
    {
        let _ = (spec, sink, fatal);
        bail!("pg-core was built without hyperliquid-marketdata support")
    }
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
