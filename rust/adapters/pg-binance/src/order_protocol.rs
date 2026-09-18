//! Strict, transport-free Binance Portfolio Margin UM order protocol.
//!
//! No credentials, signing, HTTP requests, retries or order placement live here.
//! Network timeouts MUST be represented as Unknown by the future transport, and
//! recovery must query both open and historical orders using this stable ID.

use pg_execution::{VenueOrderSnapshot, VenueOrderState};
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};

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

/// Must be populated by a *verified* exchangeInfo snapshot for this symbol.
/// No hard-coded Binance tick, lot, minimum, fee or notional is safe to assume.
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
    /// Unsigned POST form fields. Signing and timestamp are transport responsibilities.
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

/// Validates a single strategy-owned BTCUSDC order before the signed transport.
/// Hedge mode is refused until its positionSide/reduce-only safety contract exists.
/// No model is permitted to manufacture exchange prices, quantities or this intent.
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
    if intent.quantity <= rust_decimal_zero(intent) {
        return Err(ProtocolError::InvalidQuantity);
    }
    let step = filters
        .step_size
        .parse()
        .map_err(|_| ProtocolError::InvalidFilters)?;
    let min_quantity = filters
        .min_quantity
        .parse()
        .map_err(|_| ProtocolError::InvalidFilters)?;
    if step <= rust_decimal_zero(intent) || min_quantity <= rust_decimal_zero(intent) {
        return Err(ProtocolError::InvalidFilters);
    }
    if !(intent.quantity % step).is_zero() || intent.quantity < min_quantity {
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
            .parse()
            .map_err(|_| ProtocolError::InvalidFilters)?;
        let minimum_notional = filters
            .min_notional
            .parse()
            .map_err(|_| ProtocolError::InvalidFilters)?;
        if tick <= rust_decimal_zero(intent) || minimum_notional < rust_decimal_zero(intent) {
            return Err(ProtocolError::InvalidFilters);
        }
        if price <= rust_decimal_zero(intent) || !(price % tick).is_zero() {
            return Err(ProtocolError::InvalidPrice);
        }
        // A genuine reducing order may be smaller than the entry minimum.
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

// The type is inferred from OrderIntent.quantity, avoiding another public
// venue-specific Decimal representation. Zero is always quantity minus itself.
fn rust_decimal_zero(intent: &OrderIntent) -> impl PartialOrd + Copy {
    intent.quantity - intent.quantity
}

/// Validate exact identity before any venue adoption. Binance allows some
/// punctuation; our own orders deliberately use only a restricted Base32 form.
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

/// Normalize REST order data into the shared execution contract; unknown status,
/// wrong symbol, ID mismatch and inconsistent fill accounting all fail closed.
pub fn normalize_order(
    raw: RawOrder<'_>,
    expected_id: &str,
) -> Result<VenueOrderSnapshot, ProtocolError> {
    if raw.symbol != SYMBOL || raw.client_order_id != expected_id || !valid_client_order_id(expected_id)
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
        .parse()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    let filled_quantity = raw
        .executed_quantity
        .parse()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    let price = raw
        .price
        .parse()
        .map_err(|_| ProtocolError::InvalidVenueOrder)?;
    if requested_quantity <= filled_quantity - filled_quantity
        || filled_quantity < filled_quantity - filled_quantity
        || filled_quantity > requested_quantity
        || price < price - price
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

    fn order_id() -> String {
        encode_intent_bytes(&[0x42; 16])
    }

    #[test]
    fn lookup_uses_stable_id_and_symbol() {
        let id = order_id();
        let fields = lookup_by_client_id(&id).unwrap();
        assert_eq!(fields[0], ("symbol", "BTCUSDC".to_owned()));
        assert_eq!(fields[1], ("origClientOrderId", id));
        assert!(lookup_by_client_id("unowned-client-id").is_err());
    }

    #[test]
    fn completed_fill_is_recovered_not_reposted() {
        let id = order_id();
        let snapshot = normalize_order(
            RawOrder {
                symbol: SYMBOL,
                client_order_id: &id,
                order_id: "123",
                side: "BUY",
                original_quantity: "0.010",
                executed_quantity: "0.010",
                price: "80000.0",
                status: "FILLED",
            },
            &id,
        )
        .unwrap();
        assert_eq!(snapshot.state, VenueOrderState::Filled);
        assert_eq!(snapshot.client_order_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn partial_cancel_keeps_filled_quantity() {
        let id = order_id();
        let snapshot = normalize_order(
            RawOrder {
                symbol: SYMBOL,
                client_order_id: &id,
                order_id: "124",
                side: "SELL",
                original_quantity: "0.020",
                executed_quantity: "0.005",
                price: "81000",
                status: "CANCELED",
            },
            &id,
        )
        .unwrap();
        assert_eq!(snapshot.state, VenueOrderState::Canceled);
        assert_eq!(snapshot.filled_quantity.to_string(), "0.005");
    }

    #[test]
    fn mismatch_or_unknown_status_must_not_be_adopted() {
        let id = order_id();
        let mut raw = RawOrder {
            symbol: SYMBOL,
            client_order_id: &id,
            order_id: "125",
            side: "BUY",
            original_quantity: "0.010",
            executed_quantity: "0.000",
            price: "80000",
            status: "NEW",
        };
        assert!(normalize_order(raw.clone(), "pgINVALID").is_err());
        raw.status = "SOMETHING_NEW";
        assert!(normalize_order(raw.clone(), &id).is_err());
        raw.status = "FILLED";
        assert!(normalize_order(raw, &id).is_err());
    }
}
