use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, str::FromStr};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Shadow,
    Paper,
    Live,
}

impl FromStr for RunMode {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "shadow" => Ok(Self::Shadow),
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            other => Err(ConfigError::InvalidValue {
                key: "PG_RUN_MODE",
                value: other.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownPolicy {
    Preserve,
    CancelResting,
    FlattenOwned,
}

impl FromStr for ShutdownPolicy {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "preserve" => Ok(Self::Preserve),
            "cancel_resting" => Ok(Self::CancelResting),
            "flatten_owned" => Ok(Self::FlattenOwned),
            other => Err(ConfigError::InvalidValue {
                key: "PG_SHUTDOWN_POLICY",
                value: other.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncidentMode {
    Normal,
    SafeHold,
    Halt,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {key}: {value}")]
    InvalidValue { key: &'static str, value: String },
    #[error("live mode requires PG_LIVE_TRADING=true")]
    LiveTradingNotEnabled,
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub environment: String,
    pub instance_id: String,
    pub mode: RunMode,
    pub live_trading_enabled: bool,
    pub shutdown_policy: ShutdownPolicy,
    pub max_market_staleness_ms: u64,
    pub lease_ttl_seconds: u64,
}

impl RunConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let environment = env::var("PG_ENV").unwrap_or_else(|_| "dev".into());
        let instance_id =
            env::var("PG_INSTANCE_ID").map_err(|_| ConfigError::Missing("PG_INSTANCE_ID"))?;
        if instance_id.trim().is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "PG_INSTANCE_ID",
                value: instance_id,
            });
        }
        let mode = env::var("PG_RUN_MODE")
            .unwrap_or_else(|_| "shadow".into())
            .parse()?;
        let live_trading_enabled = parse_bool("PG_LIVE_TRADING", false)?;
        if mode == RunMode::Live && !live_trading_enabled {
            return Err(ConfigError::LiveTradingNotEnabled);
        }
        let shutdown_policy = env::var("PG_SHUTDOWN_POLICY")
            .unwrap_or_else(|_| "cancel_resting".into())
            .parse()?;
        let max_market_staleness_ms = parse_u64("PG_MAX_MARKET_STALENESS_MS", 3_000)?;
        let lease_ttl_seconds = parse_u64("PG_LEASE_TTL_SECONDS", 15)?;
        if max_market_staleness_ms == 0 || lease_ttl_seconds < 5 {
            return Err(ConfigError::InvalidValue {
                key: "PG_MAX_MARKET_STALENESS_MS/PG_LEASE_TTL_SECONDS",
                value: format!("{max_market_staleness_ms}/{lease_ttl_seconds}"),
            });
        }
        Ok(Self {
            environment,
            instance_id,
            mode,
            live_trading_enabled,
            shutdown_policy,
            max_market_staleness_ms,
            lease_ttl_seconds,
        })
    }

    pub fn routes_to_real_venue(&self) -> bool {
        self.mode == RunMode::Live && self.live_trading_enabled
    }
}

fn parse_bool(key: &'static str, default: bool) -> Result<bool, ConfigError> {
    let value = match env::var(key) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidValue { key, value }),
    }
}

fn parse_u64(key: &'static str, default: u64) -> Result<u64, ConfigError> {
    match env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidValue { key, value }),
        Err(_) => Ok(default),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StartupGate {
    JournalWritable,
    DatabaseReachable,
    RuntimeLeaseAcquired,
    VenueAuthenticated,
    MarketDataSynchronized,
    OpenOrdersLoaded,
    PositionsLoaded,
    OwnershipReconciled,
    UnknownStateClear,
    StrategyAllowlistLoaded,
    LiveTradingExplicitlyEnabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateStatus {
    Pending,
    Passed,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupChecklist {
    gates: BTreeMap<StartupGate, GateStatus>,
}

impl StartupChecklist {
    pub fn new() -> Self {
        Self {
            gates: all_gates()
                .into_iter()
                .map(|gate| (gate, GateStatus::Pending))
                .collect(),
        }
    }

    pub fn pass(&mut self, gate: StartupGate) {
        self.gates.insert(gate, GateStatus::Passed);
    }

    pub fn fail(&mut self, gate: StartupGate, reason: impl Into<String>) {
        self.gates.insert(gate, GateStatus::Failed(reason.into()));
    }

    pub fn status(&self, gate: StartupGate) -> &GateStatus {
        self.gates
            .get(&gate)
            .expect("all startup gates are initialized")
    }

    pub fn ready_for(&self, mode: RunMode) -> bool {
        required_gates(mode)
            .into_iter()
            .all(|gate| self.status(gate) == &GateStatus::Passed)
    }

    pub fn blocking_gates(&self, mode: RunMode) -> Vec<(StartupGate, GateStatus)> {
        required_gates(mode)
            .into_iter()
            .filter_map(|gate| {
                let status = self.status(gate).clone();
                (status != GateStatus::Passed).then_some((gate, status))
            })
            .collect()
    }
}

impl Default for StartupChecklist {
    fn default() -> Self {
        Self::new()
    }
}

fn all_gates() -> [StartupGate; 11] {
    [
        StartupGate::JournalWritable,
        StartupGate::DatabaseReachable,
        StartupGate::RuntimeLeaseAcquired,
        StartupGate::VenueAuthenticated,
        StartupGate::MarketDataSynchronized,
        StartupGate::OpenOrdersLoaded,
        StartupGate::PositionsLoaded,
        StartupGate::OwnershipReconciled,
        StartupGate::UnknownStateClear,
        StartupGate::StrategyAllowlistLoaded,
        StartupGate::LiveTradingExplicitlyEnabled,
    ]
}

pub fn required_gates(mode: RunMode) -> Vec<StartupGate> {
    match mode {
        RunMode::Shadow => vec![
            StartupGate::JournalWritable,
            StartupGate::DatabaseReachable,
            StartupGate::RuntimeLeaseAcquired,
            StartupGate::MarketDataSynchronized,
            StartupGate::StrategyAllowlistLoaded,
        ],
        RunMode::Paper => vec![
            StartupGate::JournalWritable,
            StartupGate::DatabaseReachable,
            StartupGate::RuntimeLeaseAcquired,
            StartupGate::MarketDataSynchronized,
            StartupGate::StrategyAllowlistLoaded,
        ],
        RunMode::Live => all_gates().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_requires_fewer_gates_than_live() {
        assert!(required_gates(RunMode::Shadow).len() < required_gates(RunMode::Live).len());
    }

    #[test]
    fn live_is_fail_closed_until_every_gate_passes() {
        let mut checklist = StartupChecklist::new();
        for gate in all_gates() {
            checklist.pass(gate);
        }
        checklist.fail(StartupGate::OwnershipReconciled, "unknown manual position");
        assert!(!checklist.ready_for(RunMode::Live));
        assert_eq!(checklist.blocking_gates(RunMode::Live).len(), 1);
    }
}
