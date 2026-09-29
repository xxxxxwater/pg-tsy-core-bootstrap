use anyhow::{Result, bail};
use std::future::pending;
use tokio::sync::{mpsc, oneshot};

#[cfg(feature = "telegram-control")]
use anyhow::Context;
#[cfg(feature = "telegram-control")]
use std::{collections::BTreeSet, env, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "telegram-control"), allow(dead_code))]
pub(crate) enum OperatorCommand {
    Start,
    Stop,
    Status,
    Positions,
    Orders,
    Risk,
    Refresh,
    EmergencyExit,
    Unsupported(String),
}

pub(crate) struct OperatorRequest {
    pub request_id: String,
    pub actor_user_id: i64,
    pub chat_id: i64,
    pub command: OperatorCommand,
    pub reply: oneshot::Sender<OperatorReply>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperatorReply {
    Accepted(String),
    Text(String),
    Rejected(String),
}

pub(crate) struct OperatorControl {
    receiver: mpsc::Receiver<OperatorRequest>,
    _disabled_sender: Option<mpsc::Sender<OperatorRequest>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl OperatorControl {
    pub(crate) fn from_env() -> Result<Self> {
        let (runtime_tx, runtime_rx) = mpsc::channel(32);
        if !crate::secrets::bool_env("PG_TELEGRAM_ENABLED", false)? {
            return Ok(Self {
                receiver: runtime_rx,
                _disabled_sender: Some(runtime_tx),
                tasks: Vec::new(),
            });
        }

        #[cfg(not(feature = "telegram-control"))]
        {
            let _ = runtime_tx;
            bail!(
                "PG_TELEGRAM_ENABLED=true but pg-core was built without the telegram-control feature"
            );
        }

        #[cfg(feature = "telegram-control")]
        {
            let token = crate::secrets::required_secret("TELEGRAM_BOT_TOKEN")?;
            let users = parse_id_set("TELEGRAM_ALLOWED_USER_IDS")?;
            let chats = parse_id_set("TELEGRAM_ALLOWED_CHAT_IDS")?;
            if users.is_empty() || chats.is_empty() {
                bail!(
                    "Telegram control requires non-empty TELEGRAM_ALLOWED_USER_IDS and TELEGRAM_ALLOWED_CHAT_IDS"
                );
            }
            let authorizer = pg_control::TelegramAuthorizer::new(users, chats);
            if authorizer.is_empty() {
                bail!("Telegram control authorizer is empty");
            }

            let rate_limit = pg_control::TelegramRateLimitConfig {
                normal_per_minute: env_usize("TELEGRAM_RATE_LIMIT_PER_MINUTE", 10, 1, 120)?,
                refresh_cooldown: Duration::from_secs(env_u64(
                    "TELEGRAM_REFRESH_COOLDOWN_SECONDS",
                    15,
                    1,
                    3_600,
                )?),
                emergency_cooldown: Duration::from_secs(env_u64(
                    "TELEGRAM_EMERGENCY_COOLDOWN_SECONDS",
                    60,
                    1,
                    3_600,
                )?),
                runtime_response_timeout: Duration::from_secs(env_u64(
                    "TELEGRAM_RUNTIME_RESPONSE_TIMEOUT_SECONDS",
                    30,
                    1,
                    120,
                )?),
            };
            let (telegram_tx, mut telegram_rx) = mpsc::channel(32);
            let bot_task =
                pg_control::spawn_telegram_bot(token, authorizer, rate_limit, telegram_tx);
            let bridge_tx = runtime_tx.clone();
            let bridge_task = tokio::spawn(async move {
                while let Some(envelope) = telegram_rx.recv().await {
                    let command = map_command(&envelope.request.command);
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let request = OperatorRequest {
                        request_id: envelope.request.request_id.clone(),
                        actor_user_id: envelope.request.actor_user_id,
                        chat_id: envelope.request.chat_id,
                        command,
                        reply: reply_tx,
                    };
                    if bridge_tx.send(request).await.is_err() {
                        let _ = envelope.reply.send(pg_control::ControlResponse::Rejected {
                            request_id: envelope.request.request_id,
                            reason: "runtime control channel closed".into(),
                        });
                        break;
                    }
                    let response = match reply_rx.await {
                        Ok(OperatorReply::Accepted(body)) => pg_control::ControlResponse::Text {
                            request_id: envelope.request.request_id,
                            body,
                        },
                        Ok(OperatorReply::Text(body)) => pg_control::ControlResponse::Text {
                            request_id: envelope.request.request_id,
                            body,
                        },
                        Ok(OperatorReply::Rejected(reason)) => {
                            pg_control::ControlResponse::Rejected {
                                request_id: envelope.request.request_id,
                                reason,
                            }
                        }
                        Err(_) => pg_control::ControlResponse::Rejected {
                            request_id: envelope.request.request_id,
                            reason: "runtime response channel closed".into(),
                        },
                    };
                    let _ = envelope.reply.send(response);
                }
            });

            Ok(Self {
                receiver: runtime_rx,
                _disabled_sender: None,
                tasks: vec![bot_task, bridge_task],
            })
        }
    }

    pub(crate) async fn recv(&mut self) -> OperatorRequest {
        match self.receiver.recv().await {
            Some(request) => request,
            None => pending::<OperatorRequest>().await,
        }
    }

    pub(crate) fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

#[cfg(feature = "telegram-control")]
fn map_command(command: &pg_control::ControlCommand) -> OperatorCommand {
    use pg_control::ControlCommand;
    match command {
        ControlCommand::Start => OperatorCommand::Start,
        ControlCommand::Stop => OperatorCommand::Stop,
        ControlCommand::Status => OperatorCommand::Status,
        ControlCommand::Positions => OperatorCommand::Positions,
        ControlCommand::Orders => OperatorCommand::Orders,
        ControlCommand::Risk => OperatorCommand::Risk,
        ControlCommand::Refresh => OperatorCommand::Refresh,
        ControlCommand::EmergencyExit => OperatorCommand::EmergencyExit,
        other => OperatorCommand::Unsupported(format!("{other:?}")),
    }
}

#[cfg(feature = "telegram-control")]
fn parse_id_set(name: &'static str) -> Result<Vec<i64>> {
    let raw =
        env::var(name).with_context(|| format!("missing required environment variable {name}"))?;
    let mut ids = BTreeSet::new();
    for value in raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let id = value
            .parse::<i64>()
            .with_context(|| format!("invalid integer in {name}"))?;
        if id == 0 {
            bail!("{name} cannot contain zero");
        }
        ids.insert(id);
    }
    Ok(ids.into_iter().collect())
}

#[cfg(feature = "telegram-control")]
fn env_u64(name: &'static str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = env::var(name)
        .ok()
        .map(|raw| {
            raw.parse::<u64>()
                .with_context(|| format!("invalid {name}"))
        })
        .transpose()?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        bail!("{name} must be in [{min}, {max}]");
    }
    Ok(value)
}

#[cfg(feature = "telegram-control")]
fn env_usize(name: &'static str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = env::var(name)
        .ok()
        .map(|raw| {
            raw.parse::<usize>()
                .with_context(|| format!("invalid {name}"))
        })
        .transpose()?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        bail!("{name} must be in [{min}, {max}]");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_reply_is_explicit_about_rejection() {
        assert_eq!(
            OperatorReply::Rejected("blocked".into()),
            OperatorReply::Rejected("blocked".into())
        );
    }
}
