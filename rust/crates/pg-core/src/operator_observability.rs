use crate::health::HealthState;
use anyhow::{Context, Result, bail};
use pg_marketdata::{FeedKind, FeedSpec, SubscriptionState, SubscriptionSupervisor};
use pg_observability::{
    FeedSnapshot, LeaseSnapshot, ObservabilityConfig, ObservabilityServer, OrdersSnapshot,
    ReconcileSnapshot, RuntimeObservatory, StorageSnapshot,
};
use pg_runtime::{RunConfig, RunMode, StartupChecklist, required_gates};
use pg_types::Venue;
use std::env;

pub(crate) struct OperatorObservability {
    observatory: RuntimeObservatory,
    server: ObservabilityServer,
    instance_id: String,
    fencing_token: i64,
}

impl OperatorObservability {
    pub(crate) async fn maybe_spawn(
        config: &RunConfig,
        strategy_ids: Vec<String>,
        fencing_token: i64,
    ) -> Result<Option<Self>> {
        if !enabled_from_env()? {
            return Ok(None);
        }

        let server_config = ObservabilityConfig::from_env()
            .context("invalid runtime observability configuration")?;
        let observatory = RuntimeObservatory::new(
            config,
            env!("CARGO_PKG_VERSION"),
            server_config.stale_after_ms,
            server_config.event_capacity,
        );
        observatory.set_strategy_inventory(strategy_ids);
        observatory.set_storage(StorageSnapshot {
            journal: "HEALTHY".into(),
            checkpoint_seq: None,
            journal_tail_seq: None,
            pending_dispatch: 0,
        });
        if config.mode == RunMode::Shadow {
            observatory.set_reconcile(ReconcileSnapshot {
                status: "HEALTHY".into(),
                last_success_age_ms: Some(0),
                mismatch_count: 0,
                ownership_unknown_count: 0,
            });
        }
        observatory.update_snapshot(|snapshot| {
            snapshot.capabilities.reload_script = admin_reload_configured();
        });

        let server = ObservabilityServer::spawn(server_config, observatory.clone())
            .await
            .context("failed to bind runtime observability server")?;
        tracing::info!(
            addr = %server.local_addr(),
            "runtime observability API enabled"
        );
        observatory.record_event(
            "observability.enabled",
            "info",
            "integrated pg-core observability API started",
            None,
            None,
        );

        Ok(Some(Self {
            observatory,
            server,
            instance_id: config.instance_id.clone(),
            fencing_token,
        }))
    }

    pub(crate) async fn sync(
        &self,
        health: &HealthState,
        checklist: &StartupChecklist,
        mode: RunMode,
        reconcile_clean: Option<bool>,
    ) {
        for gate in required_gates(mode) {
            self.observatory
                .set_startup_gate(gate, checklist.status(gate).clone());
        }

        let health = health.snapshot().await;
        self.observatory.set_lease(LeaseSnapshot {
            required: true,
            owned: health.lease_healthy,
            owner: health.lease_healthy.then(|| self.instance_id.clone()),
            fencing_token: health.lease_healthy.then_some(self.fencing_token),
            heartbeat_age_ms: None,
        });
        self.observatory.set_orders(OrdersSnapshot {
            open: health.open_orders as u64,
            ..OrdersSnapshot::default()
        });

        if let Some(clean) = reconcile_clean {
            self.observatory.set_reconcile(ReconcileSnapshot {
                status: if clean { "HEALTHY" } else { "DEGRADED" }.into(),
                last_success_age_ms: clean.then_some(0),
                mismatch_count: u64::from(!clean),
                ownership_unknown_count: 0,
            });
        }
    }

    pub(crate) fn sync_market_data(&self, supervisor: &SubscriptionSupervisor, now_ns: u64) {
        self.observatory
            .set_configured_feeds(feed_snapshots(supervisor, now_ns));
    }

    pub(crate) fn set_strategy_inventory(&self, strategy_ids: Vec<String>) {
        self.observatory.set_strategy_inventory(strategy_ids);
    }

    pub(crate) async fn stop(self) {
        self.server.stop().await;
    }
}

fn enabled_from_env() -> Result<bool> {
    match env::var("PG_OBSERVABILITY_ENABLED") {
        Ok(raw) => parse_enabled(&raw),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("PG_OBSERVABILITY_ENABLED must be valid UTF-8")
        }
    }
}

fn parse_enabled(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        _ => bail!(
            "invalid PG_OBSERVABILITY_ENABLED={raw}; expected true/false, 1/0, yes/no, or on/off"
        ),
    }
}

fn feed_snapshots(supervisor: &SubscriptionSupervisor, now_ns: u64) -> Vec<FeedSnapshot> {
    supervisor
        .statuses()
        .map(|(spec, status)| FeedSnapshot {
            venue: venue_name(spec.venue).into(),
            feed: feed_name(&spec.kind),
            asset: spec.asset.clone(),
            status: subscription_health(status.state).into(),
            age_ms: status
                .last_event_ns
                .map(|last| now_ns.saturating_sub(last) / 1_000_000),
            required: true,
        })
        .collect()
}

fn venue_name(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "BINANCE_PM",
        Venue::Hyperliquid => "HYPERLIQUID",
        Venue::InteractiveBrokers => "IBKR",
    }
}

fn feed_name(kind: &FeedKind) -> String {
    match kind {
        FeedKind::Trades => "trades".into(),
        FeedKind::BestBidAsk => "best_bid_ask".into(),
        FeedKind::L2Book => "l2_book".into(),
        FeedKind::Candle { interval_ns } => format!("candle:{interval_ns}"),
    }
}

fn subscription_health(state: SubscriptionState) -> &'static str {
    match state {
        SubscriptionState::Connected => "HEALTHY",
        SubscriptionState::Degraded => "DEGRADED",
        SubscriptionState::Reconnecting => "RECONNECTING",
        SubscriptionState::Stale => "STALE",
        SubscriptionState::Failed => "FAILED",
    }
}

fn admin_reload_configured() -> bool {
    env::var("PG_ADMIN_TOKEN").ok().is_some_and(|token| {
        (32..=512).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
    })
}

#[cfg(test)]
mod tests {
    use super::{feed_snapshots, parse_enabled};
    use pg_marketdata::{BestBidAsk, FeedKind, FeedSpec, MarketEvent, SubscriptionSupervisor};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    #[test]
    fn observability_boolean_parser_is_strict() {
        assert!(parse_enabled("true").unwrap());
        assert!(parse_enabled("1").unwrap());
        assert!(parse_enabled(" yes ").unwrap());
        assert!(!parse_enabled("false").unwrap());
        assert!(!parse_enabled("0").unwrap());
        assert!(!parse_enabled("").unwrap());
        assert!(parse_enabled("maybe").is_err());
    }

    #[test]
    fn feed_snapshots_preserve_supervisor_truth_and_age() {
        let spec = FeedSpec {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            kind: FeedKind::BestBidAsk,
        };
        let mut supervisor = SubscriptionSupervisor::new();
        supervisor.apply_derived_feeds([spec.clone()]);
        let before = feed_snapshots(&supervisor, 2_000_000_000);
        assert_eq!(before[0].status, "RECONNECTING");
        assert_eq!(before[0].age_ms, None);

        supervisor.observe(&MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: 1_000_000_000,
            ts_recv_ns: 1_000_000_000,
            bid_price: Decimal::from(30),
            bid_quantity: Decimal::from(10),
            ask_price: Decimal::from(31),
            ask_quantity: Decimal::from(10),
            sequence: None,
        }));
        let after = feed_snapshots(&supervisor, 1_250_000_000);
        assert_eq!(after[0].venue, "HYPERLIQUID");
        assert_eq!(after[0].feed, "best_bid_ask");
        assert_eq!(after[0].status, "HEALTHY");
        assert_eq!(after[0].age_ms, Some(250));
        assert!(after[0].required);
    }
}
