use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

#[cfg(feature = "telegram")]
pub use teloxide as telegram_sdk;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlCommand {
    Start,
    Performance,
    Status,
    Logs { lines: u16 },
    EmergencyExit,
    Scripts,
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
        match command.split('@').next().unwrap_or(command) {
            "/start" => Ok(Self::Start),
            "/performance" => Ok(Self::Performance),
            "/status" => Ok(Self::Status),
            "/logs" => {
                let lines = parts.next().unwrap_or("50");
                let lines = lines
                    .parse::<u16>()
                    .map_err(|_| CommandParseError::InvalidArgument)?;
                if !(1..=200).contains(&lines) {
                    return Err(CommandParseError::InvalidArgument);
                }
                Ok(Self::Logs { lines })
            }
            "/emergency_exit" => Ok(Self::EmergencyExit),
            "/scripts" => Ok(Self::Scripts),
            "/reload_script" => {
                let name = parts.next().ok_or(CommandParseError::InvalidArgument)?;
                if !valid_script_name(name) || parts.next().is_some() {
                    return Err(CommandParseError::InvalidArgument);
                }
                Ok(Self::ReloadScript { name: name.into() })
            }
            "/latency" => Ok(Self::Latency),
            _ => Err(CommandParseError::Unknown),
        }
    }

    pub fn mutates_runtime(&self) -> bool {
        matches!(
            self,
            Self::Start | Self::EmergencyExit | Self::ReloadScript { .. }
        )
    }
}

fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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
    fn parses_bounded_log_request() {
        assert_eq!(
            ControlCommand::parse("/logs 100").unwrap(),
            ControlCommand::Logs { lines: 100 }
        );
        assert!(ControlCommand::parse("/logs 999").is_err());
    }

    #[test]
    fn script_name_cannot_be_shell_expression() {
        assert!(ControlCommand::parse("/reload_script vwap_v4").is_ok());
        assert!(ControlCommand::parse("/reload_script 'x;rm'").is_err());
    }

    #[test]
    fn auth_requires_user_and_chat() {
        let auth = TelegramAuthorizer::new([7], [11]);
        assert!(auth.authorized(7, 11));
        assert!(!auth.authorized(7, 12));
    }
}
