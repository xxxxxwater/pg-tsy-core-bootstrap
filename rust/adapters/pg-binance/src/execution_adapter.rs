//! Strict Portfolio Margin execution adapter behind pg-risk, durable journal
//! and fencing. Construction does NOT register it in the production daemon.

use async_trait::async_trait;
use pg_execution::{
    AccountSnapshot, ExecutionAdapter, ExecutionError, OrderLocator, VenueFillSnapshot,
    VenueOrderAck, VenueOrderSnapshot, VenuePositionSnapshot,
};
use pg_types::{ExposureEffect, OrderIntent};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::{
    encode_intent_bytes,
    order_protocol::{
        OrderStyle, PositionMode, SYMBOL, SymbolFilters, prepare_order, valid_client_order_id,
    },
    rest_transport::BinanceRestClient,
    trade_history::{TradePage, UmTrade},
};

/// Convert the persisted 34-character OMS identity back to the exact native
/// 28-character Binance identity. No new UUID or client ID can be generated.
pub fn venue_id_from_durable(durable: &str) -> Result<String, ExecutionError> {
    let hex = durable
        .strip_prefix("pg")
        .filter(|hex| hex.len() == 32)
        .ok_or_else(|| ExecutionError::Conversion("invalid durable client id".into()))?;
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let section = &hex[index * 2..index * 2 + 2];
        if !section
            .bytes()
            .all(|character| character.is_ascii_hexdigit())
        {
            return Err(ExecutionError::Conversion(
                "invalid durable client id".into(),
            ));
        }
        *byte = u8::from_str_radix(section, 16)
            .map_err(|_| ExecutionError::Conversion("invalid durable client id".into()))?;
    }
    let venue_id = encode_intent_bytes(&bytes);
    if crate::durable_client_order_id(&venue_id).as_deref() != Some(durable) {
        return Err(ExecutionError::Conversion(
            "noncanonical durable client id".into(),
        ));
    }
    Ok(venue_id)
}

/// Requires current exchangeInfo filters and an independently verified one-way
/// mode. Actual write capability is separately disabled by default in REST.
pub struct BinancePmExecutionAdapter {
    rest: BinanceRestClient,
    filters: SymbolFilters,
    mode: PositionMode,
    account_scope: String,
}

impl BinancePmExecutionAdapter {
    pub fn new(
        rest: BinanceRestClient,
        filters: SymbolFilters,
        mode: PositionMode,
        account_scope: String,
    ) -> Result<Self, ExecutionError> {
        if filters.symbol != SYMBOL || !filters.symbol_trading || mode != PositionMode::OneWay {
            return Err(ExecutionError::Unsupported(
                "BTCUSDC trading status or one-way mode is unverified".into(),
            ));
        }
        let account_scope = account_scope.trim().to_owned();
        if account_scope.is_empty()
            || account_scope.len() > 128
            || !account_scope.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ExecutionError::Conversion(
                "invalid Binance account scope".into(),
            ));
        }
        Ok(Self {
            rest,
            filters,
            mode,
            account_scope: format!("binance-pm:{account_scope}"),
        })
    }
}

/// Never infer strategy ownership from an account-wide position. A hedge-mode
/// or duplicate row must hold the caller until independently reconciled.
pub fn decode_position_risk(json: &Value) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
    let records = json
        .as_array()
        .ok_or_else(|| ExecutionError::Conversion("invalid PM position response".into()))?;
    if records.len() > 1 {
        return Err(ExecutionError::Conversion(
            "ambiguous PM position rows".into(),
        ));
    }
    records
        .iter()
        .map(|row| {
            if row.get("symbol").and_then(Value::as_str) != Some(SYMBOL)
                || row.get("positionSide").and_then(Value::as_str) != Some("BOTH")
            {
                return Err(ExecutionError::Conversion(
                    "unexpected PM instrument or hedge-mode position".into(),
                ));
            }
            let quantity = row
                .get("positionAmt")
                .and_then(Value::as_str)
                .ok_or_else(|| ExecutionError::Conversion("missing PM position quantity".into()))?
                .parse::<Decimal>()
                .map_err(|_| ExecutionError::Conversion("invalid PM position quantity".into()))?;
            Ok(VenuePositionSnapshot {
                asset: SYMBOL.into(),
                quantity,
            })
        })
        .collect()
}

fn required_decimal(value: &Value, key: &str) -> Result<Decimal, ExecutionError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ExecutionError::Conversion(format!("missing Binance account field {key}")))?
        .parse::<Decimal>()
        .map_err(|_| ExecutionError::Conversion(format!("invalid Binance account field {key}")))
}

fn optional_decimal(value: &Value, key: &str) -> Result<Option<Decimal>, ExecutionError> {
    let Some(raw) = value.get(key).and_then(Value::as_str) else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    raw.parse::<Decimal>()
        .map(Some)
        .map_err(|_| ExecutionError::Conversion(format!("invalid Binance account field {key}")))
}

fn decode_account_snapshot(value: &Value) -> Result<AccountSnapshot, ExecutionError> {
    let status = value
        .get("accountStatus")
        .and_then(Value::as_str)
        .ok_or_else(|| ExecutionError::Conversion("missing Binance account status".into()))?;
    if status != "NORMAL" {
        return Err(ExecutionError::Unknown(format!(
            "Binance Portfolio Margin account status is {status}; new-exposure authority is unproven"
        )));
    }

    Ok(AccountSnapshot {
        venue: pg_types::Venue::BinancePm,
        account_id: None,
        currency: Some("USD".into()),
        account_value: Some(required_decimal(value, "accountEquity")?),
        available_funds: optional_decimal(value, "totalAvailableBalance")?,
        withdrawable: optional_decimal(value, "virtualMaxWithdrawAmount")?,
        buying_power: None,
        initial_margin: Some(required_decimal(value, "accountInitialMargin")?),
        maintenance_margin: Some(required_decimal(value, "accountMaintMargin")?),
        margin_used: None,
        gross_position_value: None,
        raw_usd: Some(required_decimal(value, "actualEquity")?),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderIdentity {
    venue_order_id: String,
    client_order_id: Option<String>,
    asset: String,
    side: pg_types::Side,
}

fn decode_order_identity(value: &Value) -> Result<OrderIdentity, ExecutionError> {
    let venue_order_id = value
        .get("orderId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| ExecutionError::Conversion("invalid Binance order identity".into()))?
        .to_string();
    let asset = value
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|symbol| *symbol == SYMBOL)
        .ok_or_else(|| ExecutionError::Conversion("unexpected Binance order symbol".into()))?
        .to_owned();
    let side = match value.get("side").and_then(Value::as_str) {
        Some("BUY") => pg_types::Side::Buy,
        Some("SELL") => pg_types::Side::Sell,
        _ => {
            return Err(ExecutionError::Conversion(
                "invalid Binance order side".into(),
            ));
        }
    };
    let native_client_id = value
        .get("clientOrderId")
        .and_then(Value::as_str)
        .ok_or_else(|| ExecutionError::Conversion("missing Binance client order id".into()))?;
    let client_order_id = if valid_client_order_id(native_client_id) {
        crate::durable_client_order_id(native_client_id)
    } else {
        None
    };
    Ok(OrderIdentity {
        venue_order_id,
        client_order_id,
        asset,
        side,
    })
}

fn bind_trade_page(
    account_scope: &str,
    page: TradePage,
    order_history: &Value,
) -> Result<Vec<VenueFillSnapshot>, ExecutionError> {
    if !page.short_page {
        return Err(ExecutionError::Unknown(
            "Binance UM userTrades page saturated at 1000 rows; complete recent fill coverage is unproven".into(),
        ));
    }
    let rows = order_history
        .as_array()
        .ok_or_else(|| ExecutionError::Conversion("invalid Binance allOrders response".into()))?;
    let mut identities = BTreeMap::new();
    for row in rows {
        let identity = decode_order_identity(row)?;
        if identities
            .insert(identity.venue_order_id.clone(), identity)
            .is_some()
        {
            return Err(ExecutionError::Conversion(
                "duplicate Binance order identity in allOrders".into(),
            ));
        }
    }

    page.trades
        .into_iter()
        .map(|trade| bind_trade(account_scope, trade, &identities))
        .collect()
}

fn bind_trade(
    account_scope: &str,
    trade: UmTrade,
    identities: &BTreeMap<String, OrderIdentity>,
) -> Result<VenueFillSnapshot, ExecutionError> {
    let identity = identities.get(&trade.venue_order_id).ok_or_else(|| {
        ExecutionError::Unknown(format!(
            "Binance trade {} cannot be bound to authenticated allOrders identity {}",
            trade.trade_id, trade.venue_order_id
        ))
    })?;
    if identity.asset != trade.symbol || identity.side != trade.side {
        return Err(ExecutionError::Unknown(format!(
            "Binance trade {} conflicts with authenticated order identity",
            trade.trade_id
        )));
    }
    let trade_id = i64::try_from(trade.trade_id)
        .map_err(|_| ExecutionError::Conversion("Binance trade id exceeds i64".into()))?;
    let trade_time_ms = i64::try_from(trade.trade_time_ms)
        .map_err(|_| ExecutionError::Conversion("Binance trade time exceeds i64".into()))?;
    Ok(VenueFillSnapshot {
        account_scope: account_scope.to_owned(),
        venue_fill_id: trade.trade_id.to_string(),
        legacy_trade_id: Some(trade_id),
        venue_order_id: trade.venue_order_id,
        client_order_id: identity.client_order_id.clone(),
        asset: trade.symbol,
        side: trade.side,
        quantity: trade.quantity,
        price: trade.price,
        commission: Some(trade.commission),
        commission_asset: Some(trade.commission_asset),
        realized_pnl: Some(trade.realized_pnl),
        trade_time_ms: Some(trade_time_ms),
        venue_time: None,
    })
}

#[async_trait]
impl ExecutionAdapter for BinancePmExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        // An exposure-increasing market order needs a separate slippage guard.
        let style = match (intent.effect, intent.limit_price) {
            (ExposureEffect::Increase, None) => {
                return Err(ExecutionError::Unsupported(
                    "unbounded exposure-increasing market order".into(),
                ));
            }
            (ExposureEffect::ReduceOnly, None) => OrderStyle::ReduceOnlyMarket,
            (_, Some(_)) => OrderStyle::PostOnly,
        };
        let prepared = prepare_order(intent, &self.filters, self.mode, style)
            .map_err(|error| ExecutionError::Conversion(format!("invalid order: {error:?}")))?;
        self.rest.submit_order(&prepared).await
    }

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        if order.asset != SYMBOL {
            return Err(ExecutionError::Conversion(
                "cancel instrument mismatch".into(),
            ));
        }
        if let Some(order_id) = order.venue_order_id
            && (order_id.is_empty() || !order_id.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(ExecutionError::Conversion("invalid venue order id".into()));
        }
        let venue_id = venue_id_from_durable(order.client_order_id)?;
        // The DurableExecution caller journals cancellation, checks fencing,
        // then re-queries this same ID to resolve cancel-vs-fill races.
        self.rest.cancel_order(&venue_id).await
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        self.rest.open_orders().await
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        decode_position_risk(&self.rest.position_risk().await?)
    }

    async fn fills(&self) -> Result<Vec<VenueFillSnapshot>, ExecutionError> {
        let page = self.rest.user_trades_page(None, 1000).await?;
        let orders = self.rest.recent_orders().await?;
        bind_trade_page(&self.account_scope, page, &orders)
    }

    async fn account_snapshot(&self) -> Result<AccountSnapshot, ExecutionError> {
        decode_account_snapshot(&self.rest.account_info().await?)
    }

    /// Signed historical query, never openOrders-only. A missing record, HTTP
    /// error or disconnected venue is unresolved, never a safe resubmit.
    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        let venue_id = venue_id_from_durable(client_order_id)?;
        let record = self.rest.query_order(&venue_id).await?;
        if record.client_order_id.as_deref() != Some(client_order_id) {
            return Err(ExecutionError::Unknown(
                "historical order identity mismatch".into(),
            ));
        }
        Ok(Some(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_id_roundtrip_does_not_generate_new_order_identity() {
        let durable = format!("pg{}", "42".repeat(16));
        let venue_id = venue_id_from_durable(&durable).unwrap();
        assert_eq!(venue_id.len(), 28);
        assert_eq!(crate::durable_client_order_id(&venue_id), Some(durable));
        assert!(venue_id_from_durable("manual-id").is_err());
        assert!(venue_id_from_durable(&format!("PG{}", "42".repeat(16))).is_err());
    }

    #[test]
    fn position_truth_requires_one_way_and_exact_symbol() {
        let correct = serde_json::json!([{
            "symbol":"BTCUSDC", "positionSide":"BOTH", "positionAmt":"-0.025"
        }]);
        let positions = decode_position_risk(&correct).unwrap();
        assert_eq!(positions[0].quantity.to_string(), "-0.025");
        assert!(
            decode_position_risk(&serde_json::json!([{
                "symbol":"BTCUSDT", "positionSide":"BOTH", "positionAmt":"1"
            }]))
            .is_err()
        );
        assert!(
            decode_position_risk(&serde_json::json!([{
                "symbol":"BTCUSDC", "positionSide":"LONG", "positionAmt":"1"
            }]))
            .is_err()
        );
        assert!(
            decode_position_risk(&serde_json::json!([{
                "symbol":"BTCUSDC", "positionSide":"BOTH", "positionAmt":"1"
            }, {
                "symbol":"BTCUSDC", "positionSide":"BOTH", "positionAmt":"1"
            }]))
            .is_err()
        );
    }

    #[test]
    fn unverified_position_mode_cannot_construct_executor() {
        let rest = BinanceRestClient::new("test-key".into(), "test-secret".into(), false).unwrap();
        let filters = SymbolFilters {
            symbol: SYMBOL.into(),
            tick_size: "0.1".into(),
            step_size: "0.001".into(),
            min_quantity: "0.001".into(),
            min_notional: "5".into(),
            symbol_trading: true,
        };
        assert!(
            BinancePmExecutionAdapter::new(
                rest,
                filters,
                PositionMode::Unverified,
                "test-account".into()
            )
            .is_err()
        );
    }

    #[test]
    fn account_snapshot_uses_portfolio_margin_equity_without_synthesizing_free_balance() {
        let snapshot = decode_account_snapshot(&serde_json::json!({
            "uniMMR": "5167.92171923",
            "accountEquity": "73.47428058",
            "actualEquity": "122607.35137903",
            "accountInitialMargin": "23.72469206",
            "accountMaintMargin": "12.50000000",
            "accountStatus": "NORMAL",
            "virtualMaxWithdrawAmount": "100.25",
            "totalAvailableBalance": "",
            "updateTime": 1657707212154_u64
        }))
        .unwrap();
        assert_eq!(snapshot.venue, pg_types::Venue::BinancePm);
        assert_eq!(snapshot.account_value, Some(Decimal::new(7347428058, 8)));
        assert_eq!(snapshot.raw_usd, Some(Decimal::new(12260735137903, 8)));
        assert_eq!(snapshot.available_funds, None);
        assert_eq!(snapshot.withdrawable, Some(Decimal::new(10025, 2)));
        assert_eq!(
            snapshot.maintenance_margin,
            Some(Decimal::new(1250000000, 8))
        );
    }

    #[test]
    fn non_normal_portfolio_margin_status_fails_closed() {
        let value = serde_json::json!({
            "accountEquity": "100",
            "actualEquity": "100",
            "accountInitialMargin": "10",
            "accountMaintMargin": "5",
            "accountStatus": "REDUCE_ONLY",
            "virtualMaxWithdrawAmount": "0",
            "totalAvailableBalance": "0"
        });
        assert!(matches!(
            decode_account_snapshot(&value),
            Err(ExecutionError::Unknown(_))
        ));
    }

    #[test]
    fn immutable_trade_page_binds_only_authenticated_order_identity() {
        let page = TradePage {
            trades: vec![UmTrade {
                symbol: SYMBOL.into(),
                trade_id: 9,
                venue_order_id: "123".into(),
                side: pg_types::Side::Buy,
                quantity: Decimal::new(5, 3),
                price: Decimal::from(80_000),
                quote_quantity: Decimal::from(400),
                commission: Decimal::new(1, 3),
                commission_asset: "USDC".into(),
                realized_pnl: Decimal::new(-125, 2),
                trade_time_ms: 1_700_000_000_000,
            }],
            next_from_id: Some(10),
            short_page: true,
        };
        let native_id = crate::encode_intent_bytes(&[0x42; 16]);
        let orders = serde_json::json!([{
            "symbol": SYMBOL,
            "orderId": 123,
            "clientOrderId": native_id,
            "side": "BUY"
        }]);
        let fills = bind_trade_page("binance-pm:test", page, &orders).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].venue_fill_id, "9");
        assert_eq!(
            fills[0].client_order_id.as_deref(),
            Some(format!("pg{}", "42".repeat(16)).as_str())
        );
        assert_eq!(fills[0].commission, Some(Decimal::new(1, 3)));
        assert_eq!(fills[0].realized_pnl, Some(Decimal::new(-125, 2)));
    }

    #[test]
    fn saturated_trade_page_fails_closed() {
        let page = TradePage {
            trades: Vec::new(),
            next_from_id: None,
            short_page: false,
        };
        assert!(matches!(
            bind_trade_page("binance-pm:test", page, &serde_json::json!([])),
            Err(ExecutionError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn cancel_rejects_foreign_symbol_without_network() {
        let rest = BinanceRestClient::new("test-key".into(), "test-secret".into(), false).unwrap();
        let filters = SymbolFilters {
            symbol: SYMBOL.into(),
            tick_size: "0.1".into(),
            step_size: "0.001".into(),
            min_quantity: "0.001".into(),
            min_notional: "5".into(),
            symbol_trading: true,
        };
        let adapter = BinancePmExecutionAdapter::new(
            rest,
            filters,
            PositionMode::OneWay,
            "test-account".into(),
        )
        .unwrap();
        let durable = format!("pg{}", "42".repeat(16));
        let wrong = OrderLocator {
            asset: "BTCUSDT",
            venue_order_id: None,
            client_order_id: &durable,
        };
        assert!(matches!(
            adapter.cancel(wrong).await,
            Err(ExecutionError::Conversion(_))
        ));
        let valid = OrderLocator {
            asset: SYMBOL,
            venue_order_id: None,
            client_order_id: &durable,
        };
        assert!(matches!(
            adapter.cancel(valid).await,
            Err(ExecutionError::Unsupported(_))
        ));
    }
}
