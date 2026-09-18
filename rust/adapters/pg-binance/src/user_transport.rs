//! Authenticated Portfolio Margin user-stream network boundary.
//!
//! Uses PM-specific `/papi/v1/listenKey` and `/pm/ws/<listenKey>`, NOT ordinary
//! `/fapi` keys or public market streams. All events are untrusted until the
//! owning daemon reconciles against durable orders. This transport does not
//! itself update OMS, own a strategy, enable trading or register in the daemon.
//! A caller MUST enter safe hold on each session start/error and reconcile
//! REST history/fills before treating order events as complete.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pg_execution::ExecutionError;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::{
    sync::mpsc,
    time::{Instant, MissedTickBehavior, interval, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::user_stream::{UserEvent, decode_user_event};

const LISTEN_KEY_ENDPOINT: &str = "https://papi.binance.com/papi/v1/listenKey";
const USER_WS_BASE: &str = "wss://fstream.binance.com/pm/ws/";
const MAX_EVENT_BYTES: usize = 64 * 1024;
const KEEPALIVE_SECS: u64 = 30 * 60;
const PING_SECS: u64 = 60;
const MAX_SESSION_SECS: u64 = 23 * 60 * 60;

/// A listen key is a credential: never place it into errors or logs.
pub struct BinanceUserStream {
    http: Client,
    api_key: String,
}

impl BinanceUserStream {
    pub fn new(api_key: String) -> Result<Self, ExecutionError> {
        if api_key.is_empty() || api_key.len() > 512 {
            return Err(ExecutionError::Authentication(
                "invalid PM stream credentials".into(),
            ));
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ExecutionError::Transport("PM stream TLS setup failed".into()))?;
        Ok(Self { http, api_key })
    }

    /// POST/PUT of a listen key are account stream management ONLY, not order
    /// mutations. A failed renewal invalidates the entire websocket session.
    async fn listen_key_request(&self, start: bool) -> Result<Option<String>, ExecutionError> {
        let request = if start {
            self.http.post(LISTEN_KEY_ENDPOINT)
        } else {
            self.http.put(LISTEN_KEY_ENDPOINT)
        };
        let response = request
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|_| ExecutionError::Transport("PM user stream management failed".into()))?;
        if response.status() != StatusCode::OK {
            return Err(ExecutionError::Unknown(
                "PM listen key invalid or renewal failed".into(),
            ));
        }
        if !start {
            return Ok(None);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| ExecutionError::Transport("PM listen key response incomplete".into()))?;
        if bytes.is_empty() || bytes.len() > 4 * 1024 {
            return Err(ExecutionError::Conversion(
                "invalid PM listen key response".into(),
            ));
        }
        let payload: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ExecutionError::Conversion("malformed PM listen key response".into()))?;
        let key = payload
            .get("listenKey")
            .and_then(Value::as_str)
            .filter(|key| valid_listen_key(key))
            .ok_or_else(|| ExecutionError::Conversion("missing PM listen key".into()))?;
        Ok(Some(key.to_owned()))
    }

    pub async fn create_listen_key(&self) -> Result<String, ExecutionError> {
        self.listen_key_request(true)
            .await?
            .ok_or_else(|| ExecutionError::Conversion("missing PM listen key".into()))
    }

    pub async fn renew_listen_key(&self) -> Result<(), ExecutionError> {
        self.listen_key_request(false).await.map(|_| ())
    }

    /// One session only: no recursive/unbounded reconnect. The operator's
    /// supervisor must back off and REST-reconcile on every returned error.
    /// All messages have bounded size and the consumer has bounded send time.
    pub async fn stream_once(&self, sink: &mpsc::Sender<UserEvent>) -> Result<(), ExecutionError> {
        require_reconcile(sink).await?;
        let key = self.create_listen_key().await?;
        let ws_url = format!("{USER_WS_BASE}{key}");
        let (mut socket, _) = timeout(Duration::from_secs(5), connect_async(&ws_url))
            .await
            .map_err(|_| ExecutionError::Transport("PM private WS connect timeout".into()))?
            .map_err(|_| ExecutionError::Transport("PM private WS handshake failed".into()))?;
        // Never include ws_url in a log/error; it contains the account token.
        let mut renew = interval(Duration::from_secs(KEEPALIVE_SECS));
        renew.set_missed_tick_behavior(MissedTickBehavior::Skip);
        renew.tick().await;
        let mut pings = interval(Duration::from_secs(PING_SECS));
        pings.set_missed_tick_behavior(MissedTickBehavior::Skip);
        pings.tick().await;
        let started = Instant::now();
        let mut last_pong = Instant::now();
        loop {
            tokio::select! {
                _ = renew.tick() => {
                    self.renew_listen_key().await?;
                }
                _ = pings.tick() => {
                    if started.elapsed() >= Duration::from_secs(MAX_SESSION_SECS) {
                        require_reconcile(sink).await?;
                        return Err(ExecutionError::Unknown("PM user stream rotation required".into()));
                    }
                    if last_pong.elapsed() > Duration::from_secs(PING_SECS * 3) {
                        require_reconcile(sink).await?;
                        return Err(ExecutionError::Unknown("PM user stream heartbeat lost".into()));
                    }
                    socket.send(Message::Ping(Vec::new().into())).await
                        .map_err(|_| ExecutionError::Transport("PM private WS ping failed".into()))?;
                }
                frame = socket.next() => {
                    let frame = frame
                        .ok_or_else(|| ExecutionError::Unknown("PM private WS disconnected".into()))?
                        .map_err(|_| ExecutionError::Unknown("PM private WS read failed".into()))?;
                    match frame {
                        Message::Text(text) => {
                            let bytes = text.as_bytes();
                            if bytes.is_empty() || bytes.len() > MAX_EVENT_BYTES {
                                require_reconcile(sink).await?;
                                return Err(ExecutionError::Unknown("PM private event oversized".into()));
                            }
                            let event = match decode_user_event(bytes) {
                                Ok(event) => event,
                                Err(_) => {
                                    require_reconcile(sink).await?;
                                    return Err(ExecutionError::Unknown("unknown PM private event; reconcile".into()));
                                }
                            };
                            send_bounded(sink, event).await?;
                        }
                        Message::Pong(_) => last_pong = Instant::now(),
                        Message::Ping(payload) => {
                            socket.send(Message::Pong(payload)).await
                                .map_err(|_| ExecutionError::Unknown("PM private WS pong failed".into()))?;
                        }
                        Message::Close(_) => {
                            require_reconcile(sink).await?;
                            return Err(ExecutionError::Unknown("PM private WS closed".into()));
                        }
                        _ => {
                            require_reconcile(sink).await?;
                            return Err(ExecutionError::Unknown("unexpected PM private WS frame".into()));
                        }
                    }
                }
            }
        }
    }
}

fn valid_listen_key(key: &str) -> bool {
    (16..=256).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

async fn send_bounded(
    sink: &mpsc::Sender<UserEvent>,
    event: UserEvent,
) -> Result<(), ExecutionError> {
    timeout(Duration::from_secs(1), sink.send(event))
        .await
        .map_err(|_| ExecutionError::Unknown("PM user event consumer backpressure".into()))?
        .map_err(|_| ExecutionError::Transport("PM user event consumer disconnected".into()))
}

async fn require_reconcile(sink: &mpsc::Sender<UserEvent>) -> Result<(), ExecutionError> {
    send_bounded(sink, UserEvent::ReconcileRequired).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_key_validation_rejects_url_or_control_injection() {
        assert!(valid_listen_key(&"a".repeat(64)));
        assert!(!valid_listen_key("short"));
        assert!(!valid_listen_key(&format!("{}?evil=1", "a".repeat(64))));
        assert!(!valid_listen_key(&format!("{}\n", "a".repeat(64))));
        assert!(!valid_listen_key(&"a".repeat(257)));
    }

    #[tokio::test]
    async fn stream_management_stays_off_until_explicitly_called() {
        let client = BinanceUserStream::new("offline-key".into());
        assert!(client.is_ok());
        assert!(BinanceUserStream::new(String::new()).is_err());
    }

    #[tokio::test]
    async fn no_session_can_start_without_emitting_reconcile_required() {
        let (tx, mut rx) = mpsc::channel(1);
        require_reconcile(&tx).await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(UserEvent::ReconcileRequired)
        ));
    }
}
