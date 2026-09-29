use crate::health::HealthState;
use anyhow::{Context, Result, bail};
use pg_observability::{
    LeaseSnapshot, ObservabilityConfig, ObservabilityServer, OrdersSnapshot, ReconcileSnapshot,
    RuntimeObservatory, StorageSnapshot,
};
use pg_runtime::{RunConfig, RunMode, StartupChecklist, required_gates};
use std::env;

pub(crate) struct OperatorObservability {
    observatory: RuntimeObservatory,
    server: ObservabilityServer,
    instance_id: String,
}

impl OperatorObservability {
    pub(crate) async fn maybe_spawn(
        config: &RunConfig,
        strategy_ids: Vec<String>,
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
            fencing_token: None,
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

fn admin_reload_configured() -> bool {
    env::var("PG_ADMIN_TOKEN").ok().is_some_and(|token| {
        (32..=512).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
    })
}

#[cfg(test)]
mod tests {
    use super::parse_enabled;

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
}
