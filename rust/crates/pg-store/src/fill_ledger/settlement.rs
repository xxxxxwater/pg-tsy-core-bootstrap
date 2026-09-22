//! Atomic settlement of a complete, separately authenticated order history.
//!
//! Callers must obtain trade IDs/fees from signed venue history and independently
//! verify the complete interval and authoritative order snapshot first. This
//! transaction cannot authenticate its caller, reconcile account positions, move
//! a trade cursor, clear SAFE_HOLD or enable order submission. On uncertainty the
//! entire batch rolls back; a retry with identical evidence is idempotent.

use std::collections::BTreeMap;

use pg_oms::{OrderRecord, OrderState};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ExecutionFill, FillLedgerError, venue_key};
use crate::{PostgresStore, RuntimeLease};

#[derive(Debug, Clone)]
pub struct CompleteOrderHistory {
    pub account_scope: String,
    pub client_order_id: String,
    pub venue_order_id: String,
    pub authoritative_filled: Decimal,
    pub authoritative_state: OrderState,
    /// Complete trade-ID history for this order, including trades committed in
    /// previous cycles. Incremental-only batches cannot prove completeness.
    pub trades: Vec<ExecutionFill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementReceipt {
    pub client_order_id: String,
    pub filled_quantity: Decimal,
    pub inserted_trades: usize,
}

fn validate_transition(
    record: &OrderRecord,
    history: &CompleteOrderHistory,
) -> Result<(), FillLedgerError> {
    if record.venue != pg_types::Venue::BinancePm
        || record.client_order_id != history.client_order_id
        || record.venue_order_id.as_deref() != Some(history.venue_order_id.as_str())
        || record.owner_strategy_id.trim().is_empty()
        || record.side.is_none()
        || record.requested_quantity <= Decimal::ZERO
        || history.account_scope.is_empty()
        || history.account_scope.len() > 128
        || history.authoritative_filled < record.filled_quantity
        || history.authoritative_filled > record.requested_quantity
        || matches!(record.state, OrderState::Created)
    {
        return Err(FillLedgerError::Conflict);
    }
    if record.is_terminal() && record.state != history.authoritative_state {
        return Err(FillLedgerError::Conflict);
    }
    let quantity = history.authoritative_filled;
    let valid = match history.authoritative_state {
        OrderState::Open => quantity.is_zero(),
        OrderState::PartiallyFilled => quantity > Decimal::ZERO && quantity < record.requested_quantity,
        OrderState::Filled => quantity == record.requested_quantity,
        OrderState::Canceled => quantity <= record.requested_quantity,
        OrderState::Rejected => quantity.is_zero(),
        _ => false,
    };
    if !valid {
        return Err(FillLedgerError::Conflict);
    }
    Ok(())
}

impl PostgresStore {
    /// All fills, OMS cumulative quantity/state and journal entries share a
    /// single fenced SQL transaction. This is *not* an admission or release gate.
    pub async fn settle_complete_order_history(
        &self,
        lease: &RuntimeLease,
        history: &CompleteOrderHistory,
    ) -> Result<SettlementReceipt, FillLedgerError> {
        if history.trades.len() > 1000 {
            return Err(FillLedgerError::Invalid("order history batch exceeds limit"));
        }
        let mut tx = self.pool().begin().await?;
        let valid: Option<i64> = sqlx::query_scalar(
            "SELECT fencing_token FROM runtime_leases WHERE lease_key=$1 AND holder_id=$2 AND fencing_token=$3 AND lease_expires_at > now() FOR UPDATE",
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
            "SELECT state FROM order_records WHERE client_order_id=$1 FOR UPDATE",
        )
        .bind(&history.client_order_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(state) = state else {
            return Err(FillLedgerError::Unowned);
        };
        let mut record: OrderRecord = serde_json::from_value(state)?;
        validate_transition(&record, history)?;

        let mut incoming = BTreeMap::new();
        let mut total = Decimal::ZERO;
        for fill in &history.trades {
            fill.validate()?;
            if fill.account_scope != history.account_scope
                || fill.venue_order_id != history.venue_order_id
                || fill.client_order_id != history.client_order_id
                || !fill.belongs_to(&record)
                || incoming.insert(fill.trade_id, fill).is_some()
            {
                return Err(FillLedgerError::Conflict);
            }
            total = total.checked_add(fill.quantity).ok_or(FillLedgerError::Conflict)?;
        }
        if total != history.authoritative_filled {
            return Err(FillLedgerError::Conflict);
        }

        let existing: Vec<Value> = sqlx::query_scalar(
            "SELECT fill_data FROM execution_fills WHERE client_order_id=$1 FOR UPDATE",
        )
        .bind(&history.client_order_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut previously_stored = BTreeMap::new();
        for value in existing {
            let fill: ExecutionFill = serde_json::from_value(value)?;
            if fill.account_scope != history.account_scope
                || incoming.get(&fill.trade_id) != Some(&&fill)
                || previously_stored.insert(fill.trade_id, fill).is_some()
            {
                return Err(FillLedgerError::Conflict);
            }
        }

        let mut inserted_trades = 0;
        for (trade_id, fill) in incoming {
            if previously_stored.contains_key(&trade_id) {
                continue;
            }
            let rows = sqlx::query(
                "INSERT INTO execution_fills (account_scope,venue,symbol,trade_id,client_order_id,fill_data,fencing_token) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
            )
            .bind(&fill.account_scope)
            .bind(venue_key(fill.venue))
            .bind(&fill.symbol)
            .bind(fill.trade_id)
            .bind(&fill.client_order_id)
            .bind(serde_json::to_value(fill)?)
            .bind(lease.fencing_token)
            .execute(&mut *tx)
            .await?;
            if rows.rows_affected() != 1 {
                return Err(FillLedgerError::Conflict);
            }
            sqlx::query(
                "INSERT INTO event_journal (event_id,stream_id,event_type,payload,fencing_token) VALUES ($1,$2,$3,$4,$5)",
            )
            .bind(Uuid::new_v4())
            .bind(format!("order:{}", history.client_order_id))
            .bind("order.fill.recorded")
            .bind(json!({"account_scope":history.account_scope,"trade_id":trade_id,"fill":fill}))
            .bind(lease.fencing_token)
            .execute(&mut *tx)
            .await?;
            inserted_trades += 1;
        }

        record.filled_quantity = total;
        record.state = history.authoritative_state;
        let rows = sqlx::query(
            "UPDATE order_records SET state=$1,fencing_token=$2,updated_at=now() WHERE client_order_id=$3 AND fencing_token <= $2",
        )
        .bind(serde_json::to_value(&record)?)
        .bind(lease.fencing_token)
        .bind(&history.client_order_id)
        .execute(&mut *tx)
        .await?;
        if rows.rows_affected() != 1 {
            return Err(FillLedgerError::FencingLost);
        }
        sqlx::query(
            "INSERT INTO event_journal (event_id,stream_id,event_type,payload,fencing_token) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(Uuid::new_v4())
        .bind(format!("order:{}", history.client_order_id))
        .bind("order.settlement.verified")
        .bind(json!({"account_scope":history.account_scope,"filled_quantity":total,"state":record.state,"trades":history.trades.len(),"inserted":inserted_trades}))
        .bind(lease.fencing_token)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SettlementReceipt {
            client_order_id: history.client_order_id.clone(),
            filled_quantity: total,
            inserted_trades,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{ExposureEffect, OrderIntent, Side, Venue};

    fn order() -> OrderRecord {
        let intent = OrderIntent {
            intent_id: Uuid::parse_str("42424242-4242-4242-4242-424242424242").unwrap(),
            strategy_id: "owned-strategy".into(),
            venue: Venue::BinancePm,
            asset: "BTCUSDC".into(),
            side: Side::Buy,
            quantity: Decimal::new(10, 3),
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        };
        let mut record = OrderRecord::from_intent(&intent);
        record.venue_order_id = Some("123".into());
        record.state = OrderState::Open;
        record
    }

    #[test]
    fn terminal_resurrection_and_unexplained_quantities_rejected() {
        let mut record = order();
        let mut history = CompleteOrderHistory {
            account_scope: "isolated".into(),
            client_order_id: record.client_order_id.clone(),
            venue_order_id: "123".into(),
            authoritative_filled: Decimal::new(5, 3),
            authoritative_state: OrderState::PartiallyFilled,
            trades: Vec::new(),
        };
        assert!(validate_transition(&record, &history).is_ok());
        record.state = OrderState::Filled;
        record.filled_quantity = record.requested_quantity;
        assert!(validate_transition(&record, &history).is_err());
        history.authoritative_filled = Decimal::new(10, 3);
        history.authoritative_state = OrderState::Open;
        assert!(validate_transition(&record, &history).is_err());
    }
}
