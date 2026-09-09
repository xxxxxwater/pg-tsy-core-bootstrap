use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use pg_runtime::{GateStatus, RunConfig, RunMode, StartupGate, required_gates};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::VecDeque,
    env,
    net::SocketAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

pub const SNAPSHOT_SCHEMA_VERSION: &str = "runtime.snapshot.v1";
pub const SNAPSHOT_ROUTE: &str = "/v1/snapshot";
pub const EVENTS_ROUTE: &str = "/v1/events";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimeSafetyState {
    Normal,
    Shadow,
    Degraded,
    SafeHold,
    Halted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub captured_at: String,
    pub stale_after_ms: u64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub environment: String,
    pub instance_id: String,
    pub mode: RunMode,
    pub version: String,
    pub uptime_sec: u64,
    pub halted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedScope {
    pub venue: Option<String>,
    pub asset: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetySnapshot {
    pub state: RuntimeSafetyState,
    pub allow_new_exposure: bool,
    pub reason: Option<String>,
    pub affected_scope: Option<AffectedScope>,
    pub effective_state: RuntimeSafetyState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseSnapshot {
    pub required: bool,
    pub owned: bool,
    pub owner: Option<String>,
    pub fencing_token: Option<i64>,
    pub heartbeat_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSnapshot {
    pub journal: String,
    pub checkpoint_seq: Option<i64>,
    pub journal_tail_seq: Option<i64>,
    pub pending_dispatch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileSnapshot {
    pub status: String,
    pub last_success_age_ms: Option<u64>,
    pub mismatch_count: u64,
    pub ownership_unknown_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedSnapshot {
    pub venue: String,
    pub feed: String,
    pub asset: String,
    pub status: String,
    pub age_ms: Option<u64>,
    pub required: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencySnapshot {
    pub market: Option<u64>,
    pub order_p50: Option<u64>,
    pub order_p99: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueSnapshot {
    pub id: String,
    pub enabled: bool,
    pub market_data: String,
    pub execution: String,
    pub reconcile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<LatencySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalSnapshot {
    pub side: String,
    pub confidence: f64,
    pub age_ms: u64,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySnapshot {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<SignalSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub venue: String,
    pub asset: String,
    pub side: String,
    pub quantity: String,
    pub notional_usd: Option<f64>,
    pub ownership: String,
    pub strategy_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSnapshot {
    pub id: String,
    pub venue: String,
    pub asset: String,
    pub side: String,
    pub status: String,
    pub filled: Option<String>,
    pub quantity: String,
    pub client_identity: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrdersSnapshot {
    pub open: u64,
    pub partial: u64,
    pub filled_recent: u64,
    pub unknown: u64,
    pub recent: Vec<OrderSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceSnapshot {
    pub pnl_today_usd: Option<f64>,
    pub gross_exposure_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesSnapshot {
    pub status: bool,
    pub logs: bool,
    pub latency: bool,
    pub performance: bool,
    pub start: bool,
    pub reload_script: bool,
    pub emergency_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupGateSnapshot {
    pub gate: StartupGate,
    pub status: GateStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupSnapshot {
    pub ready: bool,
    pub gates: Vec<StartupGateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub schema_version: String,
    pub telemetry: TelemetrySnapshot,
    pub runtime: RuntimeInfo,
    pub safety: SafetySnapshot,
    pub lease: LeaseSnapshot,
    pub storage: StorageSnapshot,
    pub reconcile: ReconcileSnapshot,
    pub feeds: Vec<FeedSnapshot>,
    pub venues: Vec<VenueSnapshot>,
    pub strategies: Vec<StrategySnapshot>,
    pub positions: Vec<PositionSnapshot>,
    pub orders: OrdersSnapshot,
    pub performance: PerformanceSnapshot,
    pub capabilities: CapabilitiesSnapshot,
    pub startup: StartupSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub seq: u64,
    pub at: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub severity: String,
    pub message: String,
    pub venue: Option<String>,
    pub asset: Option<String>,
}

struct ObservatoryInner {
    snapshot: RwLock<RuntimeSnapshot>,
    events: Mutex<VecDeque<RuntimeEvent>>,
    next_event_seq: AtomicU64,
    started_at: Instant,
    event_capacity: usize,
}

#[derive(Clone)]
pub struct RuntimeObservatory {
    inner: Arc<ObservatoryInner>,
}

impl RuntimeObservatory {
    pub fn new(
        config: &RunConfig,
        version: impl Into<String>,
        stale_after_ms: u64,
        event_capacity: usize,
    ) -> Self {
        let gates = required_gates(config.mode)
            .into_iter()
            .map(|gate| StartupGateSnapshot {
                gate,
                status: GateStatus::Pending,
            })
            .collect();
        let safety_state = RuntimeSafetyState::SafeHold;
        let snapshot = RuntimeSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION.into(),
            telemetry: TelemetrySnapshot {
                captured_at: now_rfc3339(),
                stale_after_ms,
                source: "pg-core".into(),
            },
            runtime: RuntimeInfo {
                environment: config.environment.clone(),
                instance_id: config.instance_id.clone(),
                mode: config.mode,
                version: version.into(),
                uptime_sec: 0,
                halted: false,
            },
            safety: SafetySnapshot {
                state: safety_state,
                allow_new_exposure: false,
                reason: Some("startup gates have not established runtime authority".into()),
                affected_scope: None,
                effective_state: safety_state,
            },
            lease: LeaseSnapshot {
                required: true,
                owned: false,
                owner: None,
                fencing_token: None,
                heartbeat_age_ms: None,
            },
            storage: StorageSnapshot {
                journal: "UNKNOWN".into(),
                checkpoint_seq: None,
                journal_tail_seq: None,
                pending_dispatch: 0,
            },
            reconcile: ReconcileSnapshot {
                status: "UNKNOWN".into(),
                last_success_age_ms: None,
                mismatch_count: 0,
                ownership_unknown_count: 0,
            },
            feeds: Vec::new(),
            venues: Vec::new(),
            strategies: Vec::new(),
            positions: Vec::new(),
            orders: OrdersSnapshot::default(),
            performance: PerformanceSnapshot::default(),
            capabilities: CapabilitiesSnapshot {
                status: true,
                logs: false,
                latency: false,
                performance: false,
                start: false,
                reload_script: false,
                emergency_exit: false,
            },
            startup: StartupSnapshot {
                ready: false,
                gates,
            },
        };
        Self {
            inner: Arc::new(ObservatoryInner {
                snapshot: RwLock::new(snapshot),
                events: Mutex::new(VecDeque::new()),
                next_event_seq: AtomicU64::new(1),
                started_at: Instant::now(),
                event_capacity: event_capacity.max(1),
            }),
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let mut snapshot = self
            .inner
            .snapshot
            .read()
            .expect("runtime snapshot lock poisoned")
            .clone();
        snapshot.telemetry.captured_at = now_rfc3339();
        snapshot.runtime.uptime_sec = self.inner.started_at.elapsed().as_secs();
        recompute_safety(&mut snapshot);
        snapshot
    }

    pub fn update_snapshot(&self, update: impl FnOnce(&mut RuntimeSnapshot)) {
        let mut snapshot = self
            .inner
            .snapshot
            .write()
            .expect("runtime snapshot lock poisoned");
        update(&mut snapshot);
        recompute_safety(&mut snapshot);
    }

    pub fn set_startup_gate(&self, gate: StartupGate, status: GateStatus) {
        self.update_snapshot(|snapshot| {
            if let Some(entry) = snapshot.startup.gates.iter_mut().find(|entry| entry.gate == gate) {
                entry.status = status;
            }
            snapshot.startup.ready = snapshot
                .startup
                .gates
                .iter()
                .all(|entry| entry.status == GateStatus::Passed);
        });
    }

    pub fn set_strategy_inventory(&self, strategy_ids: Vec<String>) {
        self.update_snapshot(|snapshot| {
            snapshot.strategies = strategy_ids
                .into_iter()
                .map(|id| StrategySnapshot {
                    id,
                    status: "CONFIGURED".into(),
                    asset: None,
                    position: None,
                    quantity: None,
                    signal: None,
                })
                .collect();
        });
    }

    pub fn set_configured_feeds(&self, feeds: Vec<FeedSnapshot>) {
        self.update_snapshot(|snapshot| snapshot.feeds = feeds);
    }

    pub fn set_venues(&self, venues: Vec<VenueSnapshot>) {
        self.update_snapshot(|snapshot| snapshot.venues = venues);
    }

    pub fn set_lease(&self, lease: LeaseSnapshot) {
        self.update_snapshot(|snapshot| snapshot.lease = lease);
    }

    pub fn set_storage(&self, storage: StorageSnapshot) {
        self.update_snapshot(|snapshot| snapshot.storage = storage);
    }

    pub fn set_reconcile(&self, reconcile: ReconcileSnapshot) {
        self.update_snapshot(|snapshot| snapshot.reconcile = reconcile);
    }

    pub fn set_orders(&self, orders: OrdersSnapshot) {
        self.update_snapshot(|snapshot| snapshot.orders = orders);
    }

    pub fn set_positions(&self, positions: Vec<PositionSnapshot>) {
        self.update_snapshot(|snapshot| snapshot.positions = positions);
    }

    pub fn set_performance(&self, performance: PerformanceSnapshot) {
        self.update_snapshot(|snapshot| snapshot.performance = performance);
    }

    pub fn set_halted(&self, halted: bool) {
        self.update_snapshot(|snapshot| snapshot.runtime.halted = halted);
    }

    pub fn record_event(
        &self,
        event_type: impl Into<String>,
        severity: impl Into<String>,
        message: impl Into<String>,
        venue: Option<String>,
        asset: Option<String>,
    ) -> u64 {
        let seq = self.inner.next_event_seq.fetch_add(1, Ordering::Relaxed);
        let event = RuntimeEvent {
            seq,
            at: now_rfc3339(),
            event_type: event_type.into(),
            severity: severity.into(),
            message: message.into(),
            venue,
            asset,
        };
        let mut events = self
            .inner
            .events
            .lock()
            .expect("runtime event buffer lock poisoned");
        events.push_back(event);
        while events.len() > self.inner.event_capacity {
            events.pop_front();
        }
        seq
    }

    pub fn events_after(&self, after: u64, limit: usize) -> Vec<RuntimeEvent> {
        let limit = limit.clamp(1, 200);
        self.inner
            .events
            .lock()
            .expect("runtime event buffer lock poisoned")
            .iter()
            .filter(|event| event.seq > after)
            .take(limit)
            .cloned()
            .collect()
    }
}

fn is_healthy(value: &str) -> bool {
    value.eq_ignore_ascii_case("HEALTHY")
}

fn recompute_safety(snapshot: &mut RuntimeSnapshot) {
    snapshot.startup.ready = snapshot
        .startup
        .gates
        .iter()
        .all(|entry| entry.status == GateStatus::Passed);

    if snapshot.runtime.halted {
        snapshot.safety.state = RuntimeSafetyState::Halted;
        snapshot.safety.effective_state = RuntimeSafetyState::Halted;
        snapshot.safety.allow_new_exposure = false;
        if snapshot.safety.reason.is_none() {
            snapshot.safety.reason = Some("runtime is halted".into());
        }
        return;
    }

    let mut blockers = Vec::new();
    if !snapshot.startup.ready {
        blockers.push("startup gates are not all passed".to_string());
    }
    if snapshot.lease.required && !snapshot.lease.owned {
        blockers.push("runtime lease/fencing authority is not owned".to_string());
    }
    if !is_healthy(&snapshot.storage.journal) {
        blockers.push("durable journal health is not proven".to_string());
    }
    if snapshot.orders.unknown > 0 {
        blockers.push(format!("{} order outcome(s) are unknown", snapshot.orders.unknown));
    }
    if snapshot.reconcile.mismatch_count > 0 || snapshot.reconcile.ownership_unknown_count > 0 {
        blockers.push("reconciliation or position ownership is unresolved".to_string());
    }
    if snapshot
        .feeds
        .iter()
        .any(|feed| feed.required && !is_healthy(&feed.status))
    {
        blockers.push("required market-data feed health is not proven".to_string());
    }
    if snapshot.runtime.mode == RunMode::Live {
        if !is_healthy(&snapshot.reconcile.status) {
            blockers.push("live reconciliation health is not proven".to_string());
        }
        if snapshot.venues.iter().filter(|venue| venue.enabled).any(|venue| {
            !is_healthy(&venue.execution) || !is_healthy(&venue.reconcile)
        }) {
            blockers.push("live venue execution/reconciliation health is not proven".to_string());
        }
    }

    if let Some(reason) = blockers.first() {
        snapshot.safety.state = RuntimeSafetyState::SafeHold;
        snapshot.safety.effective_state = RuntimeSafetyState::SafeHold;
        snapshot.safety.allow_new_exposure = false;
        snapshot.safety.reason = Some(reason.clone());
        return;
    }

    let degraded = snapshot
        .feeds
        .iter()
        .any(|feed| !feed.required && !is_healthy(&feed.status))
        || snapshot
            .venues
            .iter()
            .filter(|venue| venue.enabled)
            .any(|venue| !is_healthy(&venue.market_data));

    let state = if snapshot.runtime.mode == RunMode::Shadow {
        RuntimeSafetyState::Shadow
    } else if degraded {
        RuntimeSafetyState::Degraded
    } else {
        RuntimeSafetyState::Normal
    };
    snapshot.safety.state = state;
    snapshot.safety.effective_state = state;
    snapshot.safety.allow_new_exposure = true;
    snapshot.safety.reason = None;
    snapshot.safety.affected_scope = None;
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

#[derive(Debug, Error)]
pub enum ObservabilityConfigError {
    #[error("invalid PG_OBSERVABILITY_BIND: {0}")]
    InvalidBind(String),
    #[error("invalid numeric environment variable {key}: {value}")]
    InvalidNumber { key: &'static str, value: String },
    #[error("PG_OBSERVABILITY_TOKEN is required when binding observability to a non-loopback address")]
    TokenRequiredForNonLoopback,
}

#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    pub bind: SocketAddr,
    pub token: Option<String>,
    pub stale_after_ms: u64,
    pub event_capacity: usize,
}

impl ObservabilityConfig {
    pub fn from_env() -> Result<Self, ObservabilityConfigError> {
        let bind_text = env::var("PG_OBSERVABILITY_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
        let bind = bind_text
            .parse::<SocketAddr>()
            .map_err(|_| ObservabilityConfigError::InvalidBind(bind_text.clone()))?;
        let token = env::var("PG_OBSERVABILITY_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if !bind.ip().is_loopback() && token.is_none() {
            return Err(ObservabilityConfigError::TokenRequiredForNonLoopback);
        }
        Ok(Self {
            bind,
            token,
            stale_after_ms: parse_env_u64("PG_OBSERVABILITY_STALE_AFTER_MS", 10_000)?,
            event_capacity: parse_env_usize("PG_OBSERVABILITY_EVENT_CAPACITY", 1_024)?.max(1),
        })
    }
}

fn parse_env_u64(key: &'static str, default: u64) -> Result<u64, ObservabilityConfigError> {
    let Ok(value) = env::var(key) else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .map_err(|_| ObservabilityConfigError::InvalidNumber { key, value })
}

fn parse_env_usize(key: &'static str, default: usize) -> Result<usize, ObservabilityConfigError> {
    let Ok(value) = env::var(key) else {
        return Ok(default);
    };
    value
        .parse::<usize>()
        .map_err(|_| ObservabilityConfigError::InvalidNumber { key, value })
}

#[derive(Clone)]
struct ApiState {
    observatory: RuntimeObservatory,
    token: Option<String>,
}

impl ApiState {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = self.token.as_deref() else {
            return true;
        };
        let Some(value) = headers.get("authorization").and_then(|value| value.to_str().ok()) else {
            return false;
        };
        value == format!("Bearer {token}")
    }
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct EventsResponse {
    events: Vec<RuntimeEvent>,
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"code": "unauthorized", "message": "valid bearer token required"})),
    )
        .into_response()
}

async fn snapshot_handler(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if !state.authorized(&headers) {
        return unauthorized();
    }
    Json(state.observatory.snapshot()).into_response()
}

async fn events_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Response {
    if !state.authorized(&headers) {
        return unauthorized();
    }
    let events = state
        .observatory
        .events_after(query.after.unwrap_or(0), query.limit.unwrap_or(80));
    Json(EventsResponse { events }).into_response()
}

pub struct ObservabilityServer {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl ObservabilityServer {
    pub async fn spawn(
        config: ObservabilityConfig,
        observatory: RuntimeObservatory,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(config.bind).await?;
        let local_addr = listener.local_addr()?;
        let state = ApiState {
            observatory,
            token: config.token,
        };
        let app = Router::new()
            .route(SNAPSHOT_ROUTE, get(snapshot_handler))
            .route(EVENTS_ROUTE, get(events_handler))
            .with_state(state);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
            {
                tracing::error!(%error, "runtime observability server stopped with error");
            }
        });
        Ok(Self {
            local_addr,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_runtime::ShutdownPolicy;

    fn config(mode: RunMode) -> RunConfig {
        RunConfig {
            environment: "test".into(),
            instance_id: "instance-a".into(),
            mode,
            live_trading_enabled: mode == RunMode::Live,
            shutdown_policy: ShutdownPolicy::Preserve,
            max_market_staleness_ms: 3_000,
            lease_ttl_seconds: 15,
        }
    }

    #[test]
    fn startup_is_fail_closed_instead_of_faking_health() {
        let observatory = RuntimeObservatory::new(&config(RunMode::Live), "test-build", 10_000, 16);
        let snapshot = observatory.snapshot();
        assert_eq!(snapshot.safety.state, RuntimeSafetyState::SafeHold);
        assert!(!snapshot.safety.allow_new_exposure);
        assert!(!snapshot.lease.owned);
        assert_eq!(snapshot.storage.journal, "UNKNOWN");
        assert_eq!(snapshot.reconcile.status, "UNKNOWN");
    }

    #[test]
    fn all_required_evidence_can_promote_shadow_to_shadow_ready() {
        let config = config(RunMode::Shadow);
        let observatory = RuntimeObservatory::new(&config, "test-build", 10_000, 16);
        for gate in required_gates(config.mode) {
            observatory.set_startup_gate(gate, GateStatus::Passed);
        }
        observatory.set_lease(LeaseSnapshot {
            required: true,
            owned: true,
            owner: Some("instance-a".into()),
            fencing_token: Some(7),
            heartbeat_age_ms: Some(100),
        });
        observatory.set_storage(StorageSnapshot {
            journal: "HEALTHY".into(),
            checkpoint_seq: Some(12),
            journal_tail_seq: Some(14),
            pending_dispatch: 0,
        });
        let snapshot = observatory.snapshot();
        assert_eq!(snapshot.safety.state, RuntimeSafetyState::Shadow);
        assert!(snapshot.safety.allow_new_exposure);
    }

    #[test]
    fn unknown_order_forces_safe_hold() {
        let config = config(RunMode::Shadow);
        let observatory = RuntimeObservatory::new(&config, "test-build", 10_000, 16);
        for gate in required_gates(config.mode) {
            observatory.set_startup_gate(gate, GateStatus::Passed);
        }
        observatory.set_lease(LeaseSnapshot {
            required: true,
            owned: true,
            owner: Some("instance-a".into()),
            fencing_token: Some(7),
            heartbeat_age_ms: Some(100),
        });
        observatory.set_storage(StorageSnapshot {
            journal: "HEALTHY".into(),
            checkpoint_seq: None,
            journal_tail_seq: None,
            pending_dispatch: 0,
        });
        observatory.set_orders(OrdersSnapshot {
            unknown: 1,
            ..OrdersSnapshot::default()
        });
        assert_eq!(observatory.snapshot().safety.state, RuntimeSafetyState::SafeHold);
    }

    #[test]
    fn event_buffer_is_bounded_and_cursor_filtered() {
        let observatory = RuntimeObservatory::new(&config(RunMode::Shadow), "test-build", 10_000, 2);
        observatory.record_event("one", "info", "one", None, None);
        observatory.record_event("two", "info", "two", None, None);
        observatory.record_event("three", "warning", "three", None, None);
        let events = observatory.events_after(1, 80);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "two");
        assert_eq!(events[1].event_type, "three");
    }

    #[test]
    fn serialized_contract_matches_console_schema() {
        let observatory = RuntimeObservatory::new(&config(RunMode::Live), "test-build", 10_000, 16);
        let value = serde_json::to_value(observatory.snapshot()).expect("serialize snapshot");
        assert_eq!(value["schema_version"], SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(value["runtime"]["mode"], "live");
        assert_eq!(value["safety"]["state"], "SAFE_HOLD");
        assert!(value["telemetry"]["captured_at"].is_string());
    }
}
