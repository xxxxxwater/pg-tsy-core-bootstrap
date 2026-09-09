use anyhow::{Context, Result};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
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
    pub last_error: Option<String>,
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
        update(&mut self.inner.write().await);
    }
}

pub async fn serve(addr: SocketAddr, state: HealthState) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health server on {addr}"))?;
    tracing::info!(%addr, "health server listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, state).await {
                tracing::warn!(%error, "health request failed");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, state: HealthState) -> Result<()> {
    let mut buffer = [0_u8; 4096];
    let bytes = stream.read(&mut buffer).await?;
    if bytes == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
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
            "pg_policy_decisions_total {}\n"
        ),
        u8::from(snapshot.process_healthy),
        u8::from(snapshot.ready),
        u8::from(snapshot.lease_healthy),
        snapshot.feeds_total,
        snapshot.feeds_connected,
        snapshot.events_total,
        snapshot.policy_decisions_total,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
