pub mod script;

pub use script::{
    ControlAction, ControlScriptDefinition, ControlScriptRegistry, ScriptError, valid_script_name,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

#[cfg(feature = "telegram")]
pub use teloxide as telegram_sdk;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlCommand {
    Start,
    Stop,
    Performance,
    Status,
    Positions,
    Orders,
    Risk,
    Refresh,
    Logs { lines: u16 },
    EmergencyExit,
    Scripts,
    RunScript { name: String },
    ReloadScript { name: String },
    Latency,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CommandParseError {
    #[error("unknown command")]
    Unknown,
    #[error("invalid argument")]
    InvalidArgument,
}

impl ControlCommand {
    pub fn parse(input: &str) -> Result<Self, CommandParseError> {
        let mut parts = input.split_whitespace();
        let command = parts.next().ok_or(CommandParseError::Unknown)?;
        let parsed = match command.split('@').next().unwrap_or(command) {
            "/start" => Self::Start,
            "/stop" => Self::Stop,
            "/performance" => Self::Performance,
            "/status" => Self::Status,
            "/positions" => Self::Positions,
            "/orders" => Self::Orders,
            "/risk" => Self::Risk,
            "/refresh" => Self::Refresh,
            "/logs" => {
                let lines = parts.next().unwrap_or("50");
                let lines = lines
                    .parse::<u16>()
                    .map_err(|_| CommandParseError::InvalidArgument)?;
                if !(1..=200).contains(&lines) {
                    return Err(CommandParseError::InvalidArgument);
                }
                Self::Logs { lines }
            }
            "/emergency_exit" => Self::EmergencyExit,
            "/scripts" => Self::Scripts,
            "/script" => {
                let name = parts.next().ok_or(CommandParseError::InvalidArgument)?;
                if !valid_script_name(name) || parts.next().is_some() {
                    return Err(CommandParseError::InvalidArgument);
                }
                return Ok(Self::RunScript { name: name.into() });
            }
            "/reload_script" => {
                let name = parts.next().ok_or(CommandParseError::InvalidArgument)?;
                if !valid_script_name(name) || parts.next().is_some() {
                    return Err(CommandParseError::InvalidArgument);
                }
                return Ok(Self::ReloadScript { name: name.into() });
            }
            "/latency" => Self::Latency,
            _ => return Err(CommandParseError::Unknown),
        };
        if parts.next().is_some() {
            return Err(CommandParseError::InvalidArgument);
        }
        Ok(parsed)
    }

    pub fn mutates_runtime(&self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::Stop
                | Self::Refresh
                | Self::EmergencyExit
                | Self::RunScript { .. }
                | Self::ReloadScript { .. }
        )
    }

    pub fn is_emergency(&self) -> bool {
        matches!(self, Self::EmergencyExit)
    }

    pub fn is_refresh(&self) -> bool {
        matches!(self, Self::Refresh)
    }
}

#[derive(Debug, Clone)]
pub struct TelegramAuthorizer {
    allowed_user_ids: BTreeSet<i64>,
    allowed_chat_ids: BTreeSet<i64>,
}

impl TelegramAuthorizer {
    pub fn new(
        allowed_user_ids: impl IntoIterator<Item = i64>,
        allowed_chat_ids: impl IntoIterator<Item = i64>,
    ) -> Self {
        Self {
            allowed_user_ids: allowed_user_ids.into_iter().collect(),
            allowed_chat_ids: allowed_chat_ids.into_iter().collect(),
        }
    }

    pub fn authorized(&self, user_id: i64, chat_id: i64) -> bool {
        self.allowed_user_ids.contains(&user_id) && self.allowed_chat_ids.contains(&chat_id)
    }

    pub fn is_empty(&self) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_chat_ids.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    pub actor_user_id: i64,
    pub chat_id: i64,
    pub received_at_ns: u64,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlResponse {
    Accepted { request_id: String },
    Text { request_id: String, body: String },
    Rejected { request_id: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_operator_commands() {
        for (raw, expected) in [
            ("/start", ControlCommand::Start),
            ("/stop", ControlCommand::Stop),
            ("/status", ControlCommand::Status),
            ("/positions", ControlCommand::Positions),
            ("/orders", ControlCommand::Orders),
            ("/risk", ControlCommand::Risk),
            ("/refresh", ControlCommand::Refresh),
            ("/emergency_exit", ControlCommand::EmergencyExit),
        ] {
            assert_eq!(ControlCommand::parse(raw).unwrap(), expected);
        }
    }

    #[test]
    fn commands_without_arguments_reject_trailing_input() {
        assert!(ControlCommand::parse("/start now").is_err());
        assert!(ControlCommand::parse("/refresh all").is_err());
        assert!(ControlCommand::parse("/emergency_exit yes").is_err());
    }

    #[test]
    fn parses_bounded_log_request() {
        assert_eq!(
            ControlCommand::parse("/logs 100").unwrap(),
            ControlCommand::Logs { lines: 100 }
        );
        assert!(ControlCommand::parse("/logs 999").is_err());
    }

    #[test]
    fn script_name_cannot_be_shell_expression() {
        assert!(ControlCommand::parse("/script status-snapshot").is_ok());
        assert!(ControlCommand::parse("/reload_script vwap_v4").is_ok());
        assert!(ControlCommand::parse("/script 'x;rm'").is_err());
        assert!(ControlCommand::parse("/reload_script 'x;rm'").is_err());
    }

    #[test]
    fn auth_requires_user_and_chat() {
        let auth = TelegramAuthorizer::new([7], [11]);
        assert!(auth.authorized(7, 11));
        assert!(!auth.authorized(7, 12));
    }

    #[test]
    fn mutation_classification_is_fail_closed() {
        assert!(ControlCommand::Start.mutates_runtime());
        assert!(ControlCommand::Stop.mutates_runtime());
        assert!(ControlCommand::Refresh.mutates_runtime());
        assert!(ControlCommand::EmergencyExit.mutates_runtime());
        assert!(!ControlCommand::Status.mutates_runtime());
        assert!(!ControlCommand::Positions.mutates_runtime());
        assert!(!ControlCommand::Orders.mutates_runtime());
        assert!(!ControlCommand::Risk.mutates_runtime());
    }
}
