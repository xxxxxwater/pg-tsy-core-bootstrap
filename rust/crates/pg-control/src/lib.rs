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

impl ControlResponse {
    pub fn body(&self) -> String {
        match self {
            Self::Accepted { request_id } => format!("accepted · {request_id}"),
            Self::Text { body, .. } => body.clone(),
            Self::Rejected { reason, .. } => format!("rejected · {reason}"),
        }
    }
}

#[cfg(feature = "telegram")]
mod telegram_transport {
    use super::{ControlCommand, ControlRequest, ControlResponse, TelegramAuthorizer};
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use teloxide::{prelude::*, types::Message};
    use tokio::sync::{mpsc, oneshot};

    pub struct TelegramControlEnvelope {
        pub request: ControlRequest,
        pub reply: oneshot::Sender<ControlResponse>,
    }

    #[derive(Debug, Clone)]
    pub struct TelegramRateLimitConfig {
        pub normal_per_minute: usize,
        pub refresh_cooldown: Duration,
        pub emergency_cooldown: Duration,
        pub runtime_response_timeout: Duration,
    }

    impl Default for TelegramRateLimitConfig {
        fn default() -> Self {
            Self {
                normal_per_minute: 10,
                refresh_cooldown: Duration::from_secs(15),
                emergency_cooldown: Duration::from_secs(30),
                runtime_response_timeout: Duration::from_secs(30),
            }
        }
    }

    #[derive(Default)]
    struct RateState {
        normal: BTreeMap<i64, VecDeque<Instant>>,
        last_refresh: BTreeMap<i64, Instant>,
        last_emergency: BTreeMap<i64, Instant>,
    }

    struct RateLimiter {
        config: TelegramRateLimitConfig,
        state: Mutex<RateState>,
    }

    impl RateLimiter {
        fn new(config: TelegramRateLimitConfig) -> Self {
            Self {
                config,
                state: Mutex::new(RateState::default()),
            }
        }

        fn allow(&self, user_id: i64, command: &ControlCommand) -> Result<(), &'static str> {
            let now = Instant::now();
            let mut state = self.state.lock().expect("telegram rate limiter poisoned");
            // Scope the queue borrow: cooldown maps are distinct fields of the
            // same locked state and must only be accessed after it is released.
            {
                let normal = state.normal.entry(user_id).or_default();
                while normal.front().is_some_and(|instant| {
                    now.duration_since(*instant) >= Duration::from_secs(60)
                }) {
                    normal.pop_front();
                }
                if normal.len() >= self.config.normal_per_minute {
                    return Err("rate limit: 10 commands/minute");
                }
            }

            // Rejected cooldown checks never spend a normal-rate slot or move
            // the cooldown. All checks and updates share one lock.
            if command.is_refresh()
                && state.last_refresh.get(&user_id).is_some_and(|instant| {
                    now.duration_since(*instant) < self.config.refresh_cooldown
                })
            {
                return Err("rate limit: /refresh cooldown is 15s");
            }
            if command.is_emergency()
                && state.last_emergency.get(&user_id).is_some_and(|instant| {
                    now.duration_since(*instant) < self.config.emergency_cooldown
                })
            {
                return Err("rate limit: /emergency_exit cooldown is 30s");
            }
            if command.is_refresh() {
                state.last_refresh.insert(user_id, now);
            }
            if command.is_emergency() {
                state.last_emergency.insert(user_id, now);
            }
            state.normal.entry(user_id).or_default().push_back(now);
            Ok(())
        }
    }

    pub fn spawn_telegram_bot(
        token: String,
        authorizer: TelegramAuthorizer,
        rate_limit: TelegramRateLimitConfig,
        runtime: mpsc::Sender<TelegramControlEnvelope>,
    ) -> tokio::task::JoinHandle<()> {
        let authorizer = Arc::new(authorizer);
        let limiter = Arc::new(RateLimiter::new(rate_limit.clone()));
        tokio::spawn(async move {
            let bot = Bot::new(token);
            teloxide::repl(bot, move |bot: Bot, msg: Message| {
                let authorizer = authorizer.clone();
                let limiter = limiter.clone();
                let runtime = runtime.clone();
                let response_timeout = rate_limit.runtime_response_timeout;
                async move {
                    let Some(text) = msg.text() else {
                        return Ok(());
                    };
                    if !text.trim_start().starts_with('/') {
                        return Ok(());
                    }
                    let command = match ControlCommand::parse(text) {
                        Ok(command) => command,
                        Err(error) => {
                            bot.send_message(msg.chat.id, format!("invalid command · {error}"))
                                .await?;
                            return Ok(());
                        }
                    };
                    let Some(user) = msg.from.as_ref() else {
                        bot.send_message(msg.chat.id, "unauthorized").await?;
                        return Ok(());
                    };
                    let Ok(user_id) = i64::try_from(user.id.0) else {
                        bot.send_message(msg.chat.id, "unauthorized").await?;
                        return Ok(());
                    };
                    let chat_id = msg.chat.id.0;
                    if !authorizer.authorized(user_id, chat_id) {
                        bot.send_message(msg.chat.id, "unauthorized").await?;
                        return Ok(());
                    }
                    if let Err(reason) = limiter.allow(user_id, &command) {
                        bot.send_message(msg.chat.id, reason).await?;
                        return Ok(());
                    }

                    let request_id = format!("tg:{chat_id}:{}", msg.id.0);
                    let request = ControlRequest {
                        request_id: request_id.clone(),
                        actor_user_id: user_id,
                        chat_id,
                        received_at_ns: now_ns(),
                        command,
                    };
                    let (reply, response) = oneshot::channel();
                    if runtime
                        .send(TelegramControlEnvelope { request, reply })
                        .await
                        .is_err()
                    {
                        bot.send_message(msg.chat.id, "runtime unavailable").await?;
                        return Ok(());
                    }
                    let response = match tokio::time::timeout(response_timeout, response).await {
                        Ok(Ok(response)) => response,
                        Ok(Err(_)) => ControlResponse::Rejected {
                            request_id,
                            reason: "runtime response channel closed".into(),
                        },
                        Err(_) => ControlResponse::Rejected {
                            request_id,
                            reason: "runtime response timeout".into(),
                        },
                    };
                    bot.send_message(msg.chat.id, response.body()).await?;
                    Ok(())
                }
            })
            .await;
        })
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos() as u64
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn refresh_and_emergency_have_extra_cooldowns() {
            let limiter = RateLimiter::new(TelegramRateLimitConfig::default());
            assert!(limiter.allow(7, &ControlCommand::Refresh).is_ok());
            assert!(limiter.allow(7, &ControlCommand::Refresh).is_err());
            assert!(limiter.allow(8, &ControlCommand::EmergencyExit).is_ok());
            assert!(limiter.allow(8, &ControlCommand::EmergencyExit).is_err());
        }

        #[test]
        fn cooldown_rejection_does_not_consume_normal_allowance() {
            let limiter = RateLimiter::new(TelegramRateLimitConfig {
                normal_per_minute: 2,
                ..TelegramRateLimitConfig::default()
            });
            assert!(limiter.allow(7, &ControlCommand::Refresh).is_ok());
            assert!(limiter.allow(7, &ControlCommand::Refresh).is_err());
            assert!(limiter.allow(7, &ControlCommand::Status).is_ok());
            assert!(limiter.allow(7, &ControlCommand::Status).is_err());
            assert!(limiter.allow(8, &ControlCommand::Status).is_ok());
        }
    }
}

#[cfg(feature = "telegram")]
pub use telegram_transport::{
    TelegramControlEnvelope, TelegramRateLimitConfig, spawn_telegram_bot,
};

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
