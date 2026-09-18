//! Strict Portfolio Margin execution adapter behind the existing pg-risk,
//! durable journal and fencing boundaries. Construction does NOT register it
//! in the daemon or activate live trading.

use async_trait::async_trait;
use pg_execution::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenuePositionSnapshot,
};
use pg_types::{ExposureEffect, OrderIntent};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::{
    encode_intent_bytes,
    order_protocol::{OrderStyle, PositionMode, SYMBOL, SymbolFilters, prepare_order},
    rest_transport::BinanceRestClient,
};

/// The venue's 28-character ID is NOT the durable 34-character OMS ID.
/// Never resolve an unknown intent by generating a new UUID/client ID.
pub fn venue_id_from_durable(durable: &str) -> Result<String, ExecutionError> {
    let hex = durable
        .strip_prefix("pg")
        .filter(|hex| hex.len() == 32)
        .ok_or_else(|| ExecutionError::Conversion("invalid durable client id".into()))?;
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let section = &hex[index * 2..index * 2 + 2];
        if !section.bytes().all(|character| character.is_ascii_hexdigit()) {
            return Err(ExecutionError::Conversion("invalid durable client id".into()));
        }
        *byte = u8::from_str_radix(section, 16)
            .map_err(|_| ExecutionError::Conversion("invalid durable client id".into()))?;
    }
    let venue_id = encode_intent_bytes(&bytes);
    if crate::durable_client_order_id(&venue_id).as_deref() != Some(durable) {
        return Err(ExecutionError::Conversion("noncanonical durable client id".into()));
    }
    Ok(venue_id)
}

/// Requires current Binance exchangeInfo symbol filters and an independently
/// verified one-way position mode. `BinanceRestClient` defaults to no writes.
pub struct BinancePmExecutionAdapter {
    rest: BinanceRestClient,
    filters: SymbolFilters,
    mode: PositionMode,
}

impl BinancePmExecutionAdapter {
    pub fn new(
        rest: BinanceRestClient,
        filters: SymbolFilters,
        mode: PositionMode,
    ) -> Result<Self, ExecutionError> {
        if filters.symbol != SYMBOL || !filters.symbol_trading || mode != PositionMode::OneWay {
            return Err(ExecutionError::Unsupported(
                "BTCUSDC trading status or one-way mode is unverified".into(),
            ));
        }
        Ok(Self {
            rest,
            filters,
            mode,
        })
    }
}

/// Parse only the explicitly queried symbol. Never use account-wide positions
/// to infer strategy ownership, and never silently drop another position side.
pub fn decode_position_risk(json: &Value) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
    let records = json
        .as_array()
        .ok_or_else(|| ExecutionError::Conversion("invalid PM position response".into()))?;
    if records.len() > 1 {
        return Err(ExecutionError::Conversion("ambiguous PM position rows".into()));
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

#[async_trait]
impl ExecutionAdapter for BinancePmExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        // Exposure-increasing market orders are deliberately unsupported until
        // a separately audited slippage/price-collar guard is implemented.
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

    async fn cancel(&self, _order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        // This must not be registered for real trading before signed cancel and
        // terminal-history race handling are implemented and failure tested.
        Err(ExecutionError::Unsupported(
            "Binance cancellation and terminal verification not yet connected".into(),
        ))
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        self.rest.open_orders().await
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        decode_position_risk(&self.rest.position_risk().await?)
    }

    /// Use the signed historical order query, not only openOrders. A 404 or
    /// network failure is UNKNOWN and cannot justify a second POST.
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
        assert!(decode_position_risk(&serde_json::json!([{
            "symbol":"BTCUSDT", "positionSide":"BOTH", "positionAmt":"1"
        }])).is_err());
        assert!(decode_position_risk(&serde_json::json!([{
            "symbol":"BTCUSDC", "positionSide":"LONG", "positionAmt":"1"
        }])).is_err());
        assert!(decode_position_risk(&serde_json::json!([{
            "symbol":"BTCUSDC", "positionSide":"BOTH", "positionAmt":"1"
        }, {
            "symbol":"BTCUSDC", "positionSide":"BOTH", "positionAmt":"1"
        }])).is_err());
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
        assert!(BinancePmExecutionAdapter::new(rest, filters, PositionMode::Unverified).is_err());
    }
}
