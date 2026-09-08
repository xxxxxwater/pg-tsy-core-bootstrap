use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_lease_ttls() {
        assert!(matches!(validate_ttl(4), Err(StoreError::InvalidLeaseTtl)));
        assert!(validate_ttl(15).is_ok());
        assert!(matches!(
            validate_ttl(301),
            Err(StoreError::InvalidLeaseTtl)
        ));
    }
}
