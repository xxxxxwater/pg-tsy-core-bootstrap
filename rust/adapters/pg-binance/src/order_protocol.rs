//! Strict, transport-free Binance Portfolio Margin UM order protocol.
//!
//! Does not sign, send, retry, or place orders. A lost acknowledgement is
//! UNKNOWN, never a rejection; recovery must inspect open and historical truth.

use pg_execution::{VenueOrderSnapshot, VenueOrderState};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;

use crate::binance_client_order_id;

pub const SYMBOL: &str = "BTCUSDC";
pub const NEW_ORDER_PATH: &str = "/papi/v1/um/order";
pub const QUERY_ORDER_PATH: &str = "/papi/v1/um/order";
pub const ALL_ORDERS_PATH: &str = "/papi/v1/um/allOrders";
pub const TRADES_PATH: &str = "/papi/v1/um/userTrades";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    WrongInstrument,
    InvalidQuantity,
    InvalidPrice,
    InvalidFilters,
    UnsupportedPositionMode,
    UnsupportedExposure,
    InvalidClientId,
    InvalidVenueOrder,
}

/// Supply ONLY after validating the current BTCUSDC exchangeInfo filters.
#[derive(Debug, Clone)]
pub struct SymbolFilters {
    pub symbol: String,
    pub tick_size: String,
    pub step_size: String,
    pub min_quantity: String,
    pub min_notional: String,
    pub symbol_trading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    OneWay,
    Hedge,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStyle {
    PostOnly,
    Resting,
    ImmediateOrCancel,
    ReduceOnlyMarket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedOrder {
    /// Unsigned form fields. The future transport owns the signed timestamp.
    pub path: &'static str,
    pub params: Vec<(&'static str, String)>,
    pub client_id: String,
}

impl PreparedOrder {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Validate the actual persisted intent, never a model-supplied order.
/// Hedge mode is prohibited pending a separately validated position-side guard.
pub fn prepare_order(
    intent: &OrderIntent,
    filters: &SymbolFilters,
    mode: PositionMode,
    style: OrderStyle,
) -> Result<PreparedOrder, ProtocolError> {
    if intent.venue != Venue::BinancePm
        || intent.asset != SYMBOL
        || filters.symbol != SYMBOL
        || !filters.symbol_trading
    {
        return Err(ProtocolError::WrongInstrument);
    }
    if mode != PositionMode::OneWay {
        return Err(ProtocolError::UnsupportedPositionMode);
    }
    if intent.quantity <= Decimal::ZERO {
        return Err(ProtocolError::InvalidQuantity);
    }
    let step = filters
        .step_size
        .parse::<Decimal>()
        .map_err(|_| ProtocolError::InvalidFilters)?;
    let minimum_quantity = filters
        .min_quantity
        .parse::<Decimal>()
        .map_err(|_| ProtocolError::InvalidFilters)?;
    if step <= Decimal::ZERO || minimum_quantity <= Decimal::ZERO {
        return Err(ProtocolError::InvalidFilters);
    }
    if !(intent.quantity % step).is_zero() || intent.quantity < minimum_quantity {
        return Err(ProtocolError::InvalidQuantity);
    }
    let is_market = style == OrderStyle::ReduceOnlyMarket;
    if is_market && intent.effect != ExposureEffect::ReduceOnly {
        return Err(ProtocolError::UnsupportedExposure);
    }
    if !is_market && intent.limit_price.is_none() {
        return Err(ProtocolError::InvalidPrice);
    }
    let client_id = binance_client_order_id(intent);
    if !valid_client_order_id(&client_id) {
        return Err(ProtocolError::InvalidClientId);
    }
    let mut params = vec![
        ("symbol", SYMBOL.to_owned()),
        (
            "side",
            match intent.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            }
            .to_owned(),
        ),
        ("type", if is_market { "MARKET" } else { "LIMIT" }.to_owned()),
        ("positionSide", "BOTH".to_owned()),
        ("quantity", intent.quantity.to_string()),
        ("newClientOrderId", client_id.clone()),
        (
            "reduceOnly",
            (intent.effect == ExposureEffect::ReduceOnly).to_string(),
        ),
    ];
    if !is_market {
        let price = intent.limit_price.ok_or(ProtocolError::InvalidPrice)?;
        let tick = filters
            .tick_size
            .parse::<Decimal>()
            .map_err(|_| ProtocolError::InvalidFilters)?;
        let minimum_notional = filters
            .min_notional
            .parse::<Decimal>()
            .map_err(|_| ProtocolError::InvalidFilters)?;
        if tick <= Decimal::ZERO || minimum_notional < Decimal::ZERO {
            return Err(ProtocolError::InvalidFilters);
        }
        if price <= Decimal::ZERO || !(price % tick).is_zero() {
            return Err(ProtocolError::InvalidPrice);
        }
        if intent.effect == ExposureEffect::Increase
            && price * intent.quantity < minimum_notional
        {
            return Err(ProtocolError::InvalidQuantity);
        }
        params.push(("price", price.to_string()));
        params.push((
            "timeInForce",
            match style {
                OrderStyle::PostOnly => "GTX",
                OrderStyle::Resting => "GTC",
                OrderStyle::ImmediateOrCancel => "IOC",
                OrderStyle::ReduceOnlyMarket => unreachable!(),
            }
            .to_owned(),
        ));
    }
    Ok(PreparedOrder {
        path: NEW_ORDER_PATH,
        params,
        client_id,
    })
}

/// Binance allows more characters; PG itself accepts only its own full UUID ID.
pub fn valid_client_order_id(value: &str) -> bool {
    crate::decode_intent_bytes(value).is_some()
}

pub fn lookup_by_client_id(client_id: &str) -> Result<Vec<(&'static str, String)>, ProtocolError> {
    if !valid_client_order_id(client_id) {
        return Err(ProtocolError::InvalidClientId);
    }
    Ok(vec![
        ("symbol", SYMBOL.to_owned()),
        ("origClientOrderId", client_id.to_owned()),
    ])
}

#[derive(Debug, Clone)]
pub struct RawOrder<'a> {
    pub symbol: &'a str,
    pub client_order_id: &'a str,
    pub order_id: &'a str,
    pub side: &'a str,
    pub original_quantity: &'a str,
    pub executed_quantity: &'a str,
    pub price: &'a str,
    pub status: &'a str,
}

/// Use for both open and historical order reads. Unknown or contradictory
/// status/quantity is never silently interpreted as zero fills or rejection.
pub fn normalize_order(
    raw: RawOrder<'_>,
    expected_id: &str,
) -> Result<VenueOrderSnapshot, ProtocolError> {
    if raw.symbol != SYMBOL
        || raw.client_order_id != expected_id
        || !valid_client_order_id(expected_id)
        || raw.order_id.is_empty()
        || !raw.order_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ProtocolError::InvalidVenueOrder);
    }
    let side = match raw.side {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => return Err(ProtocolError::InvalidVenueOrder),
    };
    let requested_quantity = raw
        .original_quantity
        .parse::<Decimal>()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    let filled_quantity = raw
        .executed_quantity
        .parse::<Decimal>()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    let price = raw
        .price
        .parse::<Decimal>()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    if requested_quantity <= Decimal::ZERO
        || filled_quantity < Decimal::ZERO
        || filled_quantity > requested_quantity
        || price < Decimal::ZERO
    {
        return Err(ProtocolError::InvalidVenueOrder);
    }
    let state = match raw.status {
        "NEW" if filled_quantity.is_zero() => VenueOrderState::Open,
        "PARTIALLY_FILLED" if !filled_quantity.is_zero() && filled_quantity < requested_quantity => {
            VenueOrderState::PartiallyFilled
        }
        "FILLED" if filled_quantity == requested_quantity => VenueOrderState::Filled,
        "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" => VenueOrderState::Canceled,
        "REJECTED" if filled_quantity.is_zero() => VenueOrderState::Rejected,
        _ => return Err(ProtocolError::InvalidVenueOrder),
    };
    Ok(VenueOrderSnapshot {
        venue_order_id: raw.order_id.to_owned(),
        client_order_id: Some(expected_id.to_owned()),
        asset: SYMBOL.to_owned(),
        side,
        requested_quantity,
        filled_quantity,
        limit_price: if price.is_zero() { None } else { Some(price) },
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_intent_bytes;

    fn id() -> String {
        encode_intent_bytes(&[0x42; 16])
    }

    fn raw<'a>(id: &'a str, status: &'a str) -> RawOrder<'a> {
        RawOrder {
            symbol: SYMBOL,
            client_order_id: id,
            order_id: "123",
            side: "BUY",
            original_quantity: "0.010",
            executed_quantity: "0.010",
            price: "80000.0",
            status,
        }
    }

    #[test]
    fn lookup_uses_stable_id_and_symbol() {
        let client = id();
        let fields = lookup_by_client_id(&client).unwrap();
        assert_eq!(fields[0], ("symbol", "BTCUSDC".to_owned()));
        assert_eq!(fields[1], ("origClientOrderId", client));
        assert!(lookup_by_client_id("unowned").is_err());
    }

    #[test]
    fn historical_fill_is_adopted() {
        let client = id();
        let snapshot = normalize_order(raw(&client, "FILLED"), &client).unwrap();
        assert_eq!(snapshot.state, VenueOrderState::Filled);
        assert_eq!(snapshot.client_order_id.as_deref(), Some(client.as_str()));
    }

    #[test]
    fn canceled_partial_fill_is_not_zeroed() {
        let client = id();
        let mut order = raw(&client, "CANCELED");
        order.executed_quantity = "0.005";
        let snapshot = normalize_order(order, &client).unwrap();
        assert_eq!(snapshot.state, VenueOrderState::Canceled);
        assert_eq!(snapshot.filled_quantity.to_string(), "0.005");
    }

    #[test]
    fn mismatched_ids_unknown_status_and_inconsistent_fills_fail() {
        let client = id();
        assert!(normalize_order(raw(&client, "FILLED"), "bad-id").is_err());
        assert!(normalize_order(raw(&client, "NEW_STATUS"), &client).is_err());
        let mut order = raw(&client, "FILLED");
        order.executed_quantity = "0.001";
        assert!(normalize_order(order, &client).is_err());
    }
}
