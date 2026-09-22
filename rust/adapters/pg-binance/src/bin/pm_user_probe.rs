//! Explicitly opted-in, read-only PM private stream diagnostic.
//! Never submits/cancels orders or writes OMS; every reconnect is unreconciled.
//! An operator must independently verify isolated account API permissions.
use pg_binance::{user_stream::UserEvent, user_transport::BinanceUserStream};
use std::{env, process::ExitCode, time::Duration};
use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};

fn permitted(approval: &str, mode: &str, live: &str, scope: &str, key: &str) -> bool {
    approval == "APPROVE_ISOLATED_READ_ONLY_PROBE"
        && mode == "shadow"
        && live == "false"
        && !scope.trim().is_empty()
        && !key.is_empty()
}

async fn run() -> Result<(), &'static str> {
    let approval = env::var("PG_PM_READ_ONLY_PROBE_APPROVAL").unwrap_or_default();
    let mode = env::var("PG_RUN_MODE").unwrap_or_default();
    let live = env::var("PG_LIVE_TRADING").unwrap_or_default();
    let scope = env::var("PG_ISOLATED_ACCOUNT_SCOPE").unwrap_or_default();
    let key = env::var("PG_BINANCE_PM_API_KEY").unwrap_or_default();
    if !permitted(&approval, &mode, &live, &scope, &key) {
        return Err("isolated read-only probe requires explicit operator flags");
    }

    // A fresh session always emits ReconcileRequired. Do not make this an
    // admission signal: authenticated REST trade history is not consulted here.
    for attempt in 0..3 {
        let stream = BinanceUserStream::new(key.clone())
            .map_err(|_| "private stream client initialization failed")?;
        let (tx, mut rx) = mpsc::channel(128);
        let task = tokio::spawn(async move {
            timeout(Duration::from_secs(300), stream.stream_once(&tx)).await
        });
        let mut raw_order_events = 0_u64;
        let mut reconciliation_required = false;
        while let Some(event) = rx.recv().await {
            match event {
                UserEvent::ReconcileRequired => reconciliation_required = true,
                UserEvent::Order { .. } => raw_order_events += 1,
            }
        }
        // Read-only events are never applied to OMS even if no errors occur.
        let finished = task.await.map_err(|_| "private stream worker failed")?;
        if !reconciliation_required {
            return Err("missing mandatory reconnect reconciliation gate");
        }
        println!(
            "session={} untrusted_order_events={} reconciliation_required=true oms_updates=0 order_posts=0",
            attempt + 1,
            raw_order_events
        );
        match finished {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(_)) | Err(_) => {}
        }
        if attempt < 2 {
            sleep(Duration::from_secs(1_u64 << attempt)).await;
        }
    }
    Err("read-only stream exhausted bounded reconnect budget; SAFE_HOLD remains required")
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::permitted;

    #[test]
    fn unauthorised_or_live_probe_cannot_open_a_network_session() {
        assert!(!permitted("", "shadow", "false", "isolated", "key"));
        assert!(!permitted(
            "APPROVE_ISOLATED_READ_ONLY_PROBE",
            "live",
            "false",
            "isolated",
            "key"
        ));
        assert!(!permitted(
            "APPROVE_ISOLATED_READ_ONLY_PROBE",
            "shadow",
            "true",
            "isolated",
            "key"
        ));
        assert!(!permitted(
            "APPROVE_ISOLATED_READ_ONLY_PROBE",
            "shadow",
            "false",
            "",
            "key"
        ));
        assert!(permitted(
            "APPROVE_ISOLATED_READ_ONLY_PROBE",
            "shadow",
            "false",
            "isolated",
            "key"
        ));
    }
}
