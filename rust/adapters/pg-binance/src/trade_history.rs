//! Signed UM `userTrades` response decoder. History carries `orderId`, NOT
//! `clientOrderId`; match a previously persisted authenticated order first.
//! Parsing a page never proves full historical coverage or authorizes trading.

use pg_types::{Side, Venue};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::order_protocol::SYMBOL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmTrade {
    pub symbol: String,
    pub trade_id: u64,
    pub venue_order_id: String,
    pub side: Side,
    pub quantity: Decimal,
    pub price: Decimal,
    pub quote_quantity: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub realized_pnl: Decimal,
    pub trade_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    Malformed,
    UnsupportedSymbol,
    InconsistentPage,
    UnownedOrder,
    CursorOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradePage {
    pub trades: Vec<UmTrade>,
    /// Advance using the last seen ID plus one; never assume IDs are contiguous.
    pub next_from_id: Option<u64>,
    /// This page is short, not proof that *older* history is fully covered.
    pub short_page: bool,
}

/// A caller can construct this only from its separately authenticated and
/// ownership-checked durable OMS order. A client ID prefix alone proves nothing.
#[derive(Debug, Clone)]
pub struct OwnedOrder<'a> {
    pub venue: Venue,
    pub symbol: &'a str,
    pub client_order_id: &'a str,
    pub venue_order_id: &'a str,
    pub side: Side,
}

fn number(value: &Value, key: &str) -> Result<u64, HistoryError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(HistoryError::Malformed)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, HistoryError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(HistoryError::Malformed)
}

fn decimal(value: &Value, key: &str) -> Result<Decimal, HistoryError> {
    text(value, key)?
        .parse::<Decimal>()
        .map_err(|_| HistoryError::Malformed)
}

fn asset(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

pub fn decode_um_trade(value: &Value) -> Result<UmTrade, HistoryError> {
    let symbol = text(value, "symbol")?;
    if symbol != SYMBOL {
        return Err(HistoryError::UnsupportedSymbol);
    }
    if text(value, "positionSide")? != "BOTH" {
        return Err(HistoryError::Malformed);
    }
    let side = match text(value, "side")? {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => return Err(HistoryError::Malformed),
    };
    let trade_id = number(value, "id")?;
    let order_id = number(value, "orderId")?;
    let trade_time_ms = number(value, "time")?;
    let quantity = decimal(value, "qty")?;
    let price = decimal(value, "price")?;
    let quote_quantity = decimal(value, "quoteQty")?;
    let commission = decimal(value, "commission")?;
    let realized_pnl = decimal(value, "realizedPnl")?;
    let commission_asset = text(value, "commissionAsset")?;
    if order_id == 0
        || trade_time_ms == 0
        || quantity <= Decimal::ZERO
        || price <= Decimal::ZERO
        || quote_quantity <= Decimal::ZERO
        || commission < Decimal::ZERO
        || !asset(commission_asset)
        || trade_id > i64::MAX as u64
        || order_id > i64::MAX as u64
        || trade_time_ms > i64::MAX as u64
    {
        return Err(HistoryError::Malformed);
    }
    Ok(UmTrade {
        symbol: symbol.into(),
        trade_id,
        venue_order_id: order_id.to_string(),
        side,
        quantity,
        price,
        quote_quantity,
        commission,
        commission_asset: commission_asset.into(),
        realized_pnl,
        trade_time_ms,
    })
}

/// Reject truncated, reordered and overlapping *within-page* evidence. An
/// overlapping replay across pages is handled by the persisted trade-ID key.
pub fn decode_trade_page(
    response: &Value,
    requested_from_id: Option<u64>,
    requested_limit: usize,
) -> Result<TradePage, HistoryError> {
    if !(1..=1000).contains(&requested_limit) {
        return Err(HistoryError::Malformed);
    }
    let rows = response.as_array().ok_or(HistoryError::Malformed)?;
    if rows.len() > requested_limit {
        return Err(HistoryError::InconsistentPage);
    }
    let mut trades = Vec::with_capacity(rows.len());
    let mut previous_id = None;
    for row in rows {
        let trade = decode_um_trade(row)?;
        if requested_from_id.is_some_and(|from| trade.trade_id < from)
            || previous_id.is_some_and(|previous| trade.trade_id <= previous)
        {
            return Err(HistoryError::InconsistentPage);
        }
        previous_id = Some(trade.trade_id);
        trades.push(trade);
    }
    let next_from_id = match previous_id {
        Some(id) => Some(id.checked_add(1).ok_or(HistoryError::CursorOverflow)?),
        None => requested_from_id,
    };
    Ok(TradePage {
        short_page: trades.len() < requested_limit,
        trades,
        next_from_id,
    })
}

impl UmTrade {
    pub fn verified_durable_order<'a>(
        &self,
        candidate: &OwnedOrder<'a>,
    ) -> Result<&'a str, HistoryError> {
        if candidate.venue != Venue::BinancePm
            || candidate.symbol != self.symbol
            || candidate.venue_order_id != self.venue_order_id
            || candidate.side != self.side
            || candidate.client_order_id.len() != 34
            || !candidate.client_order_id.starts_with("pg")
            || !candidate.client_order_id[2..]
                .bytes()
                .all(|ch| ch.is_ascii_hexdigit())
        {
            return Err(HistoryError::UnownedOrder);
        }
        Ok(candidate.client_order_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn trade(id: u64) -> Value {
        json!({
            "symbol":"BTCUSDC", "id":id, "orderId":123,
            "side":"BUY", "price":"80000.00", "qty":"0.005",
            "quoteQty":"400.00", "realizedPnl":"-1.25",
            "commission":"0.001", "commissionAsset":"USDC",
            "time":1700000000000_u64, "buyer":true, "maker":false,
            "positionSide":"BOTH"
        })
    }

    #[test]
    fn parses_trade_and_fee_without_synthesizing_client_id() {
        let trade = decode_um_trade(&trade(9)).unwrap();
        assert_eq!(trade.quantity, Decimal::new(5, 3));
        assert_eq!(trade.commission, Decimal::new(1, 3));
        assert_eq!(trade.realized_pnl, Decimal::new(-125, 2));
        assert_eq!(trade.venue_order_id, "123");
        assert_eq!(trade.trade_id, 9);
        let candidate = OwnedOrder {
            venue: Venue::BinancePm,
            symbol: SYMBOL,
            client_order_id: "pg42424242424242424242424242424242",
            venue_order_id: "123",
            side: Side::Buy,
        };
        assert_eq!(
            trade.verified_durable_order(&candidate).unwrap(),
            candidate.client_order_id
        );
        let wrong = OwnedOrder {
            venue_order_id: "999",
            ..candidate
        };
        assert_eq!(
            trade.verified_durable_order(&wrong),
            Err(HistoryError::UnownedOrder)
        );
    }

    #[test]
    fn pagination_rejects_replay_backward_jump_and_oversize() {
        let page = decode_trade_page(&json!([trade(9), trade(13)]), Some(9), 2).unwrap();
        assert_eq!(page.next_from_id, Some(14));
        assert!(!page.short_page);
        assert!(decode_trade_page(&json!([trade(9), trade(9)]), Some(9), 2).is_err());
        assert!(decode_trade_page(&json!([trade(8)]), Some(9), 2).is_err());
        assert!(decode_trade_page(&json!([trade(9), trade(13)]), Some(9), 1).is_err());
        assert_eq!(
            decode_trade_page(&json!([]), Some(14), 100)
                .unwrap()
                .next_from_id,
            Some(14)
        );
    }

    #[test]
    fn rejects_manual_hedge_and_malformed_fee_evidence() {
        let mut value = trade(9);
        value["positionSide"] = json!("SHORT");
        assert!(decode_um_trade(&value).is_err());
        value["positionSide"] = json!("BOTH");
        value["commission"] = json!("-0.1");
        assert!(decode_um_trade(&value).is_err());
        value["commission"] = json!("0.1");
        value["symbol"] = json!("ETHUSDT");
        assert_eq!(
            decode_um_trade(&value),
            Err(HistoryError::UnsupportedSymbol)
        );
    }
}
