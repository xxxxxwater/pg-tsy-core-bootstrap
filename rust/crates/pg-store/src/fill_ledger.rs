//! A trade is immutable venue evidence, not a cumulative order-status estimate.
//! This module does not mutate OMS orders, positions or authorize new exposure.
//! All writes are fenced, ownership-checked, deduplicated and journaled atomically.

use pg_oms::OrderRecord;
use pg_types::{Side, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::{PostgresStore, RuntimeLease};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionFill {
    /// Stable, non-secret identity of the isolated PM account. Never use an API key.
    pub account_scope: String,
    pub venue: Venue,
    pub symbol: String,
    pub trade_id: i64,
    pub venue_order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub quantity: Decimal,
    pub price: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub realized_pnl: Decimal,
    pub trade_time_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillInsertOutcome {
    Inserted,
    Duplicate,
}

#[derive(Debug, Error)]
pub enum FillLedgerError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("runtime fencing lease invalid or expired")]
    FencingLost,
    #[error("invalid execution fill: {0}")]
    Invalid(&'static str),
    #[error("trade conflicts with durable order or previously stored evidence")]
    Conflict,
    #[error("durable order not found; manual/unknown trades cannot be adopted")]
    Unowned,
}

impl ExecutionFill {
    fn validate(&self) -> Result<(), FillLedgerError> {
        if self.venue != Venue::BinancePm {
            return Err(FillLedgerError::Invalid("only Binance PM is supported"));
        }
        if self.account_scope.trim().is_empty() || self.account_scope.len() > 128 {
            return Err(FillLedgerError::Invalid("invalid account scope"));
        }
        if self.symbol.is_empty()
            || self.symbol.len() > 32
            || !self
                .symbol
                .bytes()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        {
            return Err(FillLedgerError::Invalid("invalid symbol"));
        }
        if self.commission_asset.is_empty()
            || self.commission_asset.len() > 32
            || !self
                .commission_asset
                .bytes()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        {
            return Err(FillLedgerError::Invalid("invalid commission asset"));
        }
        if self.trade_id < 0 || self.trade_time_ms <= 0 {
            return Err(FillLedgerError::Invalid("invalid trade identity/time"));
        }
        if self
            .venue_order_id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .is_none()
            || self.client_order_id.len() != 34
            || !self.client_order_id.starts_with("pg")
            || !self.client_order_id[2..]
                .bytes()
                .all(|ch| ch.is_ascii_hexdigit())
        {
            return Err(FillLedgerError::Invalid("invalid durable order identity"));
        }
        if self.quantity <= Decimal::ZERO
            || self.price <= Decimal::ZERO
            || self.commission < Decimal::ZERO
        {
            return Err(FillLedgerError::Invalid(
                "invalid quantity/price/commission",
            ));
        }
        Ok(())
    }

    fn belongs_to(&self, order: &OrderRecord) -> bool {
        order.venue == self.venue
            && order.asset == self.symbol
            && order.client_order_id == self.client_order_id
            && order.venue_order_id.as_deref() == Some(self.venue_order_id.as_str())
            && order.side == Some(self.side)
            && !order.owner_strategy_id.trim().is_empty()
            && order.requested_quantity > Decimal::ZERO
            && self.quantity <= order.requested_quantity
    }
}

fn venue_key(venue: Venue) -> &'static str {
    match venue {
        Venue::BinancePm => "BINANCE_PM",
        Venue::Hyperliquid => "HYPERLIQUID",
        Venue::InteractiveBrokers => "IBKR",
    }
}

impl PostgresStore {
    /// Insert the complete trade and corresponding event in one Postgres transaction.
    /// An identical WS/REST replay is a no-op; an altered trade ID is a conflict.
    /// Locking the order serializes concurrent fills and bounds their sum.
    pub async fn insert_execution_fill(
        &self,
        lease: &RuntimeLease,
        fill: &ExecutionFill,
    ) -> Result<FillInsertOutcome, FillLedgerError> {
        fill.validate()?;
        let mut tx = self.pool().begin().await?;
        let valid: Option<i64> = sqlx::query_scalar(
            "SELECT fencing_token FROM runtime_leases WHERE lease_key = $1 AND holder_id = $2 AND fencing_token = $3 AND lease_expires_at > now() FOR UPDATE",
        )
        .bind(&lease.lease_key)
        .bind(&lease.holder_id)
        .bind(lease.fencing_token)
        .fetch_optional(&mut *tx)
        .await?;
        if valid.is_none() {
            return Err(FillLedgerError::FencingLost);
        }

        let state: Option<Value> = sqlx::query_scalar(
            "SELECT state FROM order_records WHERE client_order_id = $1 FOR UPDATE",
        )
        .bind(&fill.client_order_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(state) = state else {
            return Err(FillLedgerError::Unowned);
        };
        let order: OrderRecord = serde_json::from_value(state)?;
        if !fill.belongs_to(&order) {
            return Err(FillLedgerError::Conflict);
        }

        let previous: Option<Value> = sqlx::query_scalar(
            "SELECT fill_data FROM execution_fills WHERE account_scope = $1 AND venue = $2 AND symbol = $3 AND trade_id = $4 FOR UPDATE",
        )
        .bind(&fill.account_scope)
        .bind(venue_key(fill.venue))
        .bind(&fill.symbol)
        .bind(fill.trade_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            let previous: ExecutionFill = serde_json::from_value(previous)?;
            if previous == *fill {
                return Ok(FillInsertOutcome::Duplicate);
            }
            return Err(FillLedgerError::Conflict);
        }

        let previous: Vec<Value> = sqlx::query_scalar(
            "SELECT fill_data FROM execution_fills WHERE client_order_id = $1 FOR UPDATE",
        )
        .bind(&fill.client_order_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut total = Decimal::ZERO;
        for value in previous {
            let old: ExecutionFill = serde_json::from_value(value)?;
            if old.account_scope != fill.account_scope
                || old.venue != fill.venue
                || old.symbol != fill.symbol
                || old.venue_order_id != fill.venue_order_id
                || old.side != fill.side
            {
                return Err(FillLedgerError::Conflict);
            }
            total = total
                .checked_add(old.quantity)
                .ok_or(FillLedgerError::Conflict)?;
        }
        total = total
            .checked_add(fill.quantity)
            .ok_or(FillLedgerError::Conflict)?;
        if total > order.requested_quantity {
            return Err(FillLedgerError::Conflict);
        }

        let encoded = serde_json::to_value(fill)?;
        let inserted = sqlx::query(
            "INSERT INTO execution_fills (account_scope, venue, symbol, trade_id, client_order_id, fill_data, fencing_token) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
        )
        .bind(&fill.account_scope)
        .bind(venue_key(fill.venue))
        .bind(&fill.symbol)
        .bind(fill.trade_id)
        .bind(&fill.client_order_id)
        .bind(&encoded)
        .bind(lease.fencing_token)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(FillLedgerError::Conflict);
        }
        sqlx::query(
            "INSERT INTO event_journal (event_id, stream_id, event_type, payload, fencing_token) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(Uuid::new_v4())
        .bind(format!("order:{}", fill.client_order_id))
        .bind("order.fill.recorded")
        .bind(json!({ "account_scope": fill.account_scope, "symbol": fill.symbol, "trade_id": fill.trade_id, "fill": fill }))
        .bind(lease.fencing_token)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FillInsertOutcome::Inserted)
    }

    /// Read-only ledger sum; MUST be compared with authenticated order history
    /// before an OMS quantity change or a SAFE_HOLD release.
    pub async fn recorded_fill_quantity(
        &self,
        lease: &RuntimeLease,
        client_order_id: &str,
    ) -> Result<Decimal, FillLedgerError> {
        self.assert_lease(lease)
            .await
            .map_err(|_| FillLedgerError::FencingLost)?;
        let stored: Vec<Value> = sqlx::query_scalar(
            "SELECT fill_data FROM execution_fills WHERE client_order_id = $1 ORDER BY trade_id",
        )
        .bind(client_order_id)
        .fetch_all(self.pool())
        .await?;
        let mut quantity = Decimal::ZERO;
        for value in stored {
            let fill: ExecutionFill = serde_json::from_value(value)?;
            quantity = quantity
                .checked_add(fill.quantity)
                .ok_or(FillLedgerError::Conflict)?;
        }
        Ok(quantity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{ExposureEffect, OrderIntent};

    fn fixture() -> (ExecutionFill, OrderRecord) {
        let intent = OrderIntent {
            intent_id: Uuid::parse_str("42424242-4242-4242-4242-424242424242").unwrap(),
            strategy_id: "isolated-strategy".into(),
            asset: "BTCUSDC".into(),
            venue: Venue::BinancePm,
            side: Side::Buy,
            quantity: Decimal::new(10, 3),
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        };
        let mut order = OrderRecord::from_intent(&intent);
        order.venue_order_id = Some("123".into());
        let fill = ExecutionFill {
            account_scope: "isolated-account".into(),
            venue: Venue::BinancePm,
            symbol: "BTCUSDC".into(),
            trade_id: 9,
            venue_order_id: "123".into(),
            client_order_id: order.client_order_id.clone(),
            side: Side::Buy,
            quantity: Decimal::new(5, 3),
            price: Decimal::from(80_000),
            commission: Decimal::new(1, 3),
            commission_asset: "USDC".into(),
            realized_pnl: Decimal::ZERO,
            trade_time_ms: 1_700_000_000_000,
        };
        (fill, order)
    }

    #[test]
    fn strict_order_ownership_and_manual_rejection() {
        let (fill, mut order) = fixture();
        assert!(fill.validate().is_ok());
        assert!(fill.belongs_to(&order));
        order.venue_order_id = Some("999".into());
        assert!(!fill.belongs_to(&order));
        order.venue_order_id = Some("123".into());
        order.side = Some(Side::Sell);
        assert!(!fill.belongs_to(&order));
    }

    #[test]
    fn malformed_and_overfilled_trade_is_blocked() {
        let (mut fill, order) = fixture();
        fill.quantity = Decimal::new(11, 3);
        assert!(!fill.belongs_to(&order));
        fill.quantity = Decimal::ZERO;
        assert!(fill.validate().is_err());
        fill.quantity = Decimal::new(5, 3);
        fill.account_scope.clear();
        assert!(fill.validate().is_err());
    }
}
