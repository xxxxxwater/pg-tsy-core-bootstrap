use anyhow::{Context, Result};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{RwLock, mpsc},
};

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub process_healthy: bool,
    pub ready: bool,
    pub mode: String,
    pub lease_healthy: bool,
    pub feeds_total: usize,
    pub feeds_connected: usize,
    pub events_total: u64,
    pub policy_decisions_total: u64,
    /// Orders the runtime currently believes are resting at a venue.
    pub open_orders: usize,
    /// Orders journaled through the durable execution path.
    pub orders_journaled_total: u64,
    /// Startup gates that are still pending or failed for the active run mode.
    /// Empty once the runtime is ready.
    pub blocking_gates: Vec<String>,
    pub last_error: Option<String>,
}

/// The local operator surface can only request strategy validation/reload.
/// It never submits, cancels or flattens an order directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    ReloadStrategies,
}

impl HealthSnapshot {
    pub fn booting(mode: impl Into<String>) -> Self {
        Self {
            process_healthy: true,
            ready: false,
            mode: mode.into(),
            lease_healthy: false,
            feeds_total: 0,
            feeds_connected: 0,
            events_total: 0,
            policy_decisions_total: 0,
            open_orders: 0,
            orders_journaled_total: 0,
            blocking_gates: Vec::new(),
            last_error: None,
        }
    }
}

#[derive(Clone)]
pub struct HealthState {
    inner: Arc<RwLock<HealthSnapshot>>,
}

impl HealthState {
    pub fn new(snapshot: HealthSnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    pub async fn snapshot(&self) -> HealthSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn mutate(&self, update: impl FnOnce(&mut HealthSnapshot)) {
        let mut guard = self.inner.write().await;
        update(&mut guard);
    }
}

/// An absent or invalid secret always disables administrative operations. Do
/// not log the value, derive a fallback from instance IDs, or accept an empty
/// bearer value. This is independent of the health listener's bind address.
fn admin_token_from_env() -> Option<Arc<str>> {
    match std::env::var("PG_ADMIN_TOKEN") {
        Ok(token) if valid_admin_token(&token) => Some(Arc::from(token)),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
            tracing::warn!("PG_ADMIN_TOKEN is invalid; admin reload disabled");
            None
        }
        Err(std::env::VarError::NotPresent) => None,
    }
}

fn valid_admin_token(token: &str) -> bool {
    (32..=512).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Parse only a complete HTTP header block. Reject missing/duplicate auth
/// headers, non-Bearer schemes and token mismatches. No substring matching.
fn authorized_admin_request(request: &str, expected: &str) -> bool {
    let Some((head, _)) = request.split_once("\r\n\r\n") else {
        return false;
    };
    let mut supplied: Option<&str> = None;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        if name.trim().eq_ignore_ascii_case("authorization") {
            if supplied.is_some() {
                return false;
            }
            supplied = value.trim().strip_prefix("Bearer ");
            if supplied.is_none() {
                return false;
            }
        }
    }
    let Some(supplied) = supplied else {
        return false;
    };
    // Compare every byte of equal-length tokens; do not short-circuit on the
    // first mismatch. Length is bounded by valid_admin_token at configuration.
    let supplied = supplied.as_bytes();
    let expected = expected.as_bytes();
    if supplied.len() != expected.len() {
        return false;
    }
    let difference = supplied
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |acc, (left, right)| acc | (left ^ right));
    difference == 0
}

pub async fn serve(
    addr: SocketAddr,
    state: HealthState,
    control: Option<mpsc::Sender<ControlCommand>>,
) -> Result<()> {
    let admin_token = admin_token_from_env();
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health server on {addr}"))?;
    tracing::info!(%addr, admin_reload_enabled = admin_token.is_some(), "health server listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        let control = control.clone();
        let admin_token = admin_token.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, state, control, admin_token).await {
                tracing::warn!(%error, "health request failed");
            }
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    state: HealthState,
    control: Option<mpsc::Sender<ControlCommand>>,
    admin_token: Option<Arc<str>>,
) -> Result<()> {
    let mut buffer = [0_u8; 4096];
    let bytes = stream.read(&mut buffer).await?;
    if bytes == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let mut request_line = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = request_line.next().unwrap_or("GET");
    let path = request_line.next().unwrap_or("/");

    if path == "/admin/reload" {
        let (status, content_type, body) = if method != "POST" {
            (
                "405 Method Not Allowed",
                "application/json",
                "{\"accepted\":false,\"reason\":\"use POST\"}\n".to_string(),
            )
        } else if admin_token.is_none() {
            (
                "403 Forbidden",
                "application/json",
                "{\"accepted\":false,\"reason\":\"admin reload disabled\"}\n".to_string(),
            )
        } else if !admin_token
            .as_deref()
            .is_some_and(|token| authorized_admin_request(&request, token))
        {
            (
                "401 Unauthorized",
                "application/json",
                "{\"accepted\":false,\"reason\":\"admin authentication required\"}\n".to_string(),
            )
        } else {
            match control {
                Some(sender) => match sender.send(ControlCommand::ReloadStrategies).await {
                    Ok(()) => (
                        "202 Accepted",
                        "application/json",
                        "{\"accepted\":true,\"command\":\"reload_strategies\"}\n".to_string(),
                    ),
                    Err(_) => (
                        "503 Service Unavailable",
                        "application/json",
                        "{\"accepted\":false,\"reason\":\"runtime control channel closed\"}\n"
                            .to_string(),
                    ),
                },
                None => (
                    "503 Service Unavailable",
                    "application/json",
                    "{\"accepted\":false,\"reason\":\"control channel not attached\"}\n"
                        .to_string(),
                ),
            }
        };
        write_response(&mut stream, status, content_type, &body).await?;
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let (status, content_type, body) = match path {
        "/healthz" => (
            if snapshot.process_healthy {
                "200 OK"
            } else {
                "503 Service Unavailable"
            },
            "application/json",
            serde_json::to_string(&snapshot)?,
        ),
        "/readyz" => (
            if snapshot.ready {
                "200 OK"
            } else {
                "503 Service Unavailable"
            },
            "application/json",
            serde_json::to_string(&snapshot)?,
        ),
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", metrics(&snapshot)),
        _ => ("404 Not Found", "text/plain", "not found\n".into()),
    };

    write_response(&mut stream, status, content_type, &body).await
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn metrics(snapshot: &HealthSnapshot) -> String {
    format!(
        concat!(
            "# TYPE pg_process_healthy gauge\n",
            "pg_process_healthy {}\n",
            "# TYPE pg_ready gauge\n",
            "pg_ready {}\n",
            "# TYPE pg_runtime_lease_healthy gauge\n",
            "pg_runtime_lease_healthy {}\n",
            "# TYPE pg_market_feeds_total gauge\n",
            "pg_market_feeds_total {}\n",
            "# TYPE pg_market_feeds_connected gauge\n",
            "pg_market_feeds_connected {}\n",
            "# TYPE pg_market_events_total counter\n",
            "pg_market_events_total {}\n",
            "# TYPE pg_policy_decisions_total counter\n",
            "pg_policy_decisions_total {}\n",
            "# TYPE pg_open_orders gauge\n",
            "pg_open_orders {}\n",
            "# TYPE pg_orders_journaled_total counter\n",
            "pg_orders_journaled_total {}\n",
            "# TYPE pg_startup_gates_blocking gauge\n",
            "pg_startup_gates_blocking {}\n"
        ),
        u8::from(snapshot.process_healthy),
        u8::from(snapshot.ready),
        u8::from(snapshot.lease_healthy),
        snapshot.feeds_total,
        snapshot.feeds_connected,
        snapshot.events_total,
        snapshot.policy_decisions_total,
        snapshot.open_orders,
        snapshot.orders_journaled_total,
        snapshot.blocking_gates.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

    #[test]
    fn prometheus_snapshot_is_machine_readable() {
        let mut snapshot = HealthSnapshot::booting("shadow");
        snapshot.ready = true;
        snapshot.lease_healthy = true;
        snapshot.feeds_total = 4;
        snapshot.feeds_connected = 4;
        let text = metrics(&snapshot);
        assert!(text.contains("pg_ready 1"));
        assert!(text.contains("pg_market_feeds_connected 4"));
    }

    #[test]
    fn admin_token_requires_long_printable_secret() {
        assert!(!valid_admin_token(""));
        assert!(!valid_admin_token("short"));
        assert!(!valid_admin_token(&format!("{}\n", TEST_TOKEN)));
        assert!(valid_admin_token(TEST_TOKEN));
    }

    #[test]
    fn admin_auth_rejects_malformed_missing_and_duplicate_headers() {
        let correct = format!(
            "POST /admin/reload HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TEST_TOKEN}\r\n\r\n"
        );
        assert!(authorized_admin_request(&correct, TEST_TOKEN));
        assert!(authorized_admin_request(
            &correct.replace("Authorization", "authorization"),
            TEST_TOKEN
        ));
        assert!(!authorized_admin_request(
            &correct.replace("\r\n\r\n", ""),
            TEST_TOKEN
        ));
        assert!(!authorized_admin_request(
            &correct.replace("Authorization: Bearer", "Authorization: Basic"),
            TEST_TOKEN
        ));
        assert!(!authorized_admin_request(
            &correct.replace(TEST_TOKEN, "different"),
            TEST_TOKEN
        ));
        assert!(!authorized_admin_request(
            &correct.replace("Host: localhost\r\n", ""),
            "different"
        ));
        let duplicate = correct.replace(
            "Host: localhost\r\n",
            &format!("Authorization: Bearer {TEST_TOKEN}\r\n"),
        );
        assert!(!authorized_admin_request(&duplicate, TEST_TOKEN));
    }

    async fn request_with_control(
        request: &str,
        token: Option<&str>,
    ) -> (String, Option<ControlCommand>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let token = token.map(Arc::<str>::from);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle(
                stream,
                HealthState::new(HealthSnapshot::booting("shadow")),
                Some(sender),
                token,
            )
            .await
            .unwrap();
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        (
            String::from_utf8(response).unwrap(),
            receiver.try_recv().ok(),
        )
    }

    #[tokio::test]
    async fn reload_is_disabled_without_token_even_with_control_channel() {
        let (response, command) = request_with_control(
            "POST /admin/reload HTTP/1.1\r\nHost: localhost\r\n\r\n",
            None,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert_eq!(command, None);
    }

    #[tokio::test]
    async fn reload_requires_bearer_and_never_dispatches_on_wrong_token() {
        let (response, command) = request_with_control(
            "POST /admin/reload HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer wrong\r\n\r\n",
            Some(TEST_TOKEN),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert_eq!(command, None);
    }

    #[tokio::test]
    async fn authorized_reload_dispatches_only_the_reload_command() {
        let request = format!(
            "POST /admin/reload HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TEST_TOKEN}\r\n\r\n"
        );
        let (response, command) = request_with_control(&request, Some(TEST_TOKEN)).await;
        assert!(response.starts_with("HTTP/1.1 202 Accepted"));
        assert_eq!(command, Some(ControlCommand::ReloadStrategies));
    }

    #[tokio::test]
    async fn health_is_available_without_admin_token() {
        let (response, command) =
            request_with_control("GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n", None).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(command, None);
    }
}
