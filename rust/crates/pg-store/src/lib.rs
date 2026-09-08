use pg_oms::OrderRecord;
use pg_reconcile::{ReconcileReport, VenuePosition};
use pg_types::Venue;
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle, time::MissedTickBehavior};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("runtime lease is currently owned by another live instance")]
    LeaseUnavailable,
    #[error("runtime fencing token is no longer valid")]
    FencingLost,
    #[error("lease TTL must be between 5 and 300 seconds")]
    InvalidLeaseTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLease {
    pub lease_key: String,
    pub holder_id: String,
    pub fencing_token: i64,
    pub ttl_seconds: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseHealth {
    Healthy,
    Lost(String),
    Stopped,
}

pub struct LeaseHeartbeatHandle {
    shutdown: watch::Sender<bool>,
    health: watch::Receiver<LeaseHealth>,
    task: JoinHandle<()>,
}

impl LeaseHeartbeatHandle {
    pub fn health(&self) -> watch::Receiver<LeaseHealth> {
        self.health.clone()
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::raw_sql(include_str!("../migrations/0001_runtime.sql"))
            .execute(&self.pool)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0002_trading_state.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn acquire_lease(
        &self,
        lease_key: &str,
        holder_id: &str,
        ttl_seconds: i32,
    ) -> Result<RuntimeLease, StoreError> {
        validate_ttl(ttl_seconds)?;
        let fencing_token = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO runtime_leases (
                lease_key, holder_id, fencing_token, lease_expires_at, updated_at
            )
            VALUES ($1, $2, 1, now() + make_interval(secs => $3), now())
            ON CONFLICT (lease_key) DO UPDATE SET
                holder_id = EXCLUDED.holder_id,
                fencing_token = CASE
                    WHEN runtime_leases.holder_id = EXCLUDED.holder_id
                    THEN runtime_leases.fencing_token
                    ELSE runtime_leases.fencing_token + 1
                END,
                lease_expires_at = EXCLUDED.lease_expires_at,
                updated_at = now()
            WHERE runtime_leases.lease_expires_at <= now()
               OR runtime_leases.holder_id = EXCLUDED.holder_id
            RETURNING fencing_token
            "#,
        )
        .bind(lease_key)
        .bind(holder_id)
        .bind(ttl_seconds)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::LeaseUnavailable)?;

        Ok(RuntimeLease {
            lease_key: lease_key.into(),
            holder_id: holder_id.into(),
            fencing_token,
            ttl_seconds,
        })
    }

    pub async fn assert_lease(&self, lease: &RuntimeLease) -> Result<(), StoreError> {
        let valid = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM runtime_leases
                WHERE lease_key = $1
                  AND holder_id = $2
                  AND fencing_token = $3
                  AND lease_expires_at > now()
            )
            "#,
        )
        .bind(&lease.lease_key)
        .bind(&lease.holder_id)
        .bind(lease.fencing_token)
        .fetch_one(&self.pool)
        .await?;
        if !valid {
            return Err(StoreError::FencingLost);
        }
        Ok(())
    }

    pub async fn renew_lease(&self, lease: &RuntimeLease) -> Result<(), StoreError> {
        validate_ttl(lease.ttl_seconds)?;
        let result = sqlx::query(
            r#"
            UPDATE runtime_leases
            SET lease_expires_at = now() + make_interval(secs => $4), updated_at = now()
            WHERE lease_key = $1
              AND holder_id = $2
              AND fencing_token = $3
              AND lease_expires_at > now()
            "#,
        )
        .bind(&lease.lease_key)
        .bind(&lease.holder_id)
        .bind(lease.fencing_token)
        .bind(lease.ttl_seconds)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::FencingLost);
        }
        Ok(())
    }

    pub fn spawn_lease_heartbeat(&self, lease: RuntimeLease) -> LeaseHeartbeatHandle {
        let store = self.clone();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let (health_tx, health_rx) = watch::channel(LeaseHealth::Healthy);
        let renew_every = Duration::from_secs((lease.ttl_seconds as u64 / 3).max(1));

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(renew_every);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            interval.tick().await;

            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            let _ = health_tx.send(LeaseHealth::Stopped);
                            return;
                        }
                    }
                    _ = interval.tick() => {
                        if let Err(error) = store.renew_lease(&lease).await {
                            let _ = health_tx.send(LeaseHealth::Lost(error.to_string()));
                            return;
                        }
                    }
                }
            }
        });

        LeaseHeartbeatHandle {
            shutdown: shutdown_tx,
            health: health_rx,
            task,
        }
    }

    pub async fn release_lease(&self, lease: &RuntimeLease) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            DELETE FROM runtime_leases
            WHERE lease_key = $1 AND holder_id = $2 AND fencing_token = $3
            "#,
        )
        .bind(&lease.lease_key)
        .bind(&lease.holder_id)
        .bind(lease.fencing_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::FencingLost);
        }
        Ok(())
    }

    pub async fn append_event(
        &self,
        stream_id: &str,
        event_type: &str,
        payload: &Value,
        fencing_token: i64,
    ) -> Result<i64, StoreError> {
        let event_id = Uuid::new_v4();
        let sequence = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO event_journal (
                event_id, stream_id, event_type, payload, fencing_token
            ) VALUES ($1, $2, $3, $4, $5)
            RETURNING sequence
            "#,
        )
        .bind(event_id)
        .bind(stream_id)
        .bind(event_type)
        .bind(payload)
        .bind(fencing_token)
        .fetch_one(&self.pool)
        .await?;
        Ok(sequence)
    }

    pub async fn save_checkpoint(
        &self,
        stream_id: &str,
        journal_sequence: i64,
        state: &Value,
        fencing_token: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO checkpoints (
                stream_id, journal_sequence, state, fencing_token, updated_at
            ) VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (stream_id) DO UPDATE SET
                journal_sequence = EXCLUDED.journal_sequence,
                state = EXCLUDED.state,
                fencing_token = EXCLUDED.fencing_token,
                updated_at = now()
            WHERE checkpoints.journal_sequence <= EXCLUDED.journal_sequence
              AND checkpoints.fencing_token <= EXCLUDED.fencing_token
            "#,
        )
        .bind(stream_id)
        .bind(journal_sequence)
        .bind(state)
        .bind(fencing_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_checkpoint(
        &self,
        stream_id: &str,
    ) -> Result<Option<(i64, Value, i64)>, StoreError> {
        let row = sqlx::query_as::<_, (i64, Value, i64)>(
            r#"
            SELECT journal_sequence, state, fencing_token
            FROM checkpoints
            WHERE stream_id = $1
            "#,
        )
        .bind(stream_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn save_order_record(
        &self,
        record: &OrderRecord,
        fencing_token: i64,
    ) -> Result<(), StoreError> {
        let state = serde_json::to_value(record)?;
        let result = sqlx::query(
            r#"
            INSERT INTO order_records (
                client_order_id, venue, asset, owner_strategy_id, state, fencing_token, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (client_order_id) DO UPDATE SET
                venue = EXCLUDED.venue,
                asset = EXCLUDED.asset,
                owner_strategy_id = EXCLUDED.owner_strategy_id,
                state = EXCLUDED.state,
                fencing_token = EXCLUDED.fencing_token,
                updated_at = now()
            WHERE order_records.fencing_token <= EXCLUDED.fencing_token
            "#,
        )
        .bind(&record.client_order_id)
        .bind(venue_key(record.venue))
        .bind(&record.asset)
        .bind(&record.owner_strategy_id)
        .bind(state)
        .bind(fencing_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::FencingLost);
        }
        Ok(())
    }

    pub async fn load_orders_for_venue(
        &self,
        venue: Venue,
    ) -> Result<Vec<OrderRecord>, StoreError> {
        let states = sqlx::query_scalar::<_, Value>(
            r#"
            SELECT state FROM order_records
            WHERE venue = $1
            ORDER BY updated_at, client_order_id
            "#,
        )
        .bind(venue_key(venue))
        .fetch_all(&self.pool)
        .await?;
        states
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub async fn save_position_state(
        &self,
        position: &VenuePosition,
        fencing_token: i64,
    ) -> Result<(), StoreError> {
        let state = serde_json::to_value(position)?;
        let result = sqlx::query(
            r#"
            INSERT INTO position_ownership (
                venue, asset, position_state, observed_quantity, fencing_token, updated_at
            ) VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (venue, asset) DO UPDATE SET
                position_state = EXCLUDED.position_state,
                observed_quantity = EXCLUDED.observed_quantity,
                fencing_token = EXCLUDED.fencing_token,
                updated_at = now()
            WHERE position_ownership.fencing_token <= EXCLUDED.fencing_token
            "#,
        )
        .bind(venue_key(position.venue))
        .bind(&position.asset)
        .bind(state)
        .bind(position.quantity.to_string())
        .bind(fencing_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::FencingLost);
        }
        Ok(())
    }

    pub async fn load_position_states(
        &self,
        venue: Venue,
    ) -> Result<Vec<VenuePosition>, StoreError> {
        let states = sqlx::query_scalar::<_, Value>(
            r#"
            SELECT position_state FROM position_ownership
            WHERE venue = $1
            ORDER BY asset
            "#,
        )
        .bind(venue_key(venue))
        .fetch_all(&self.pool)
        .await?;
        states
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub async fn save_reconcile_report(
        &self,
        venue: Venue,
        report: &ReconcileReport,
        fencing_token: i64,
    ) -> Result<Uuid, StoreError> {
        let reconcile_id = Uuid::new_v4();
        let report = serde_json::to_value(report)?;
        sqlx::query(
            r#"
            INSERT INTO reconcile_runs (
                reconcile_id, venue, report, fencing_token
            ) VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(reconcile_id)
        .bind(venue_key(venue))
        .bind(report)
        .bind(fencing_token)
        .execute(&self.pool)
        .await?;
        Ok(reconcile_id)
    }

    pub async fn audit_command(
        &self,
        request_id: &str,
        actor_user_id: i64,
        chat_id: i64,
        command: &Value,
        outcome: &str,
        fencing_token: Option<i64>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO command_audit (
                request_id, actor_user_id, chat_id, command, outcome, fencing_token
            ) VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (request_id) DO NOTHING
            "#,
        )
        .bind(request_id)
        .bind(actor_user_id)
        .bind(chat_id)
        .bind(command)
        .bind(outcome)
        .bind(fencing_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn validate_ttl(ttl_seconds: i32) -> Result<(), StoreError> {
    if !(5..=300).contains(&ttl_seconds) {
        return Err(StoreError::InvalidLeaseTtl);
    }
    Ok(())
}

fn venue_key(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "BINANCE_PM",
        Venue::Hyperliquid => "HYPERLIQUID",
        Venue::InteractiveBrokers => "IBKR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_lease_ttls() {
        assert!(matches!(
            validate_ttl(4),
            Err(StoreError::InvalidLeaseTtl)
        ));
        assert!(validate_ttl(15).is_ok());
        assert!(matches!(
            validate_ttl(301),
            Err(StoreError::InvalidLeaseTtl)
        ));
    }

    #[test]
    fn venue_keys_are_stable_for_persistence() {
        assert_eq!(venue_key(Venue::Hyperliquid), "HYPERLIQUID");
        assert_eq!(venue_key(Venue::InteractiveBrokers), "IBKR");
    }
}
