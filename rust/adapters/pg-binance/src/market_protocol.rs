//! Strict, read-only Binance BTCUSDC USD-M public wire protocol.
//!
//! Accepts a single raw or combined-stream JSON message. This module does NOT
//! open a websocket or fetch a snapshot; a future transport owns connection,
//! receive timestamps, reconnect, resubscribe, and call to the depth bridge.

use pg_marketdata::{
    AggressorSide, BestBidAsk, MarketEvent, TradeTick,
    binance_depth::{BinanceDepthDelta, BinanceDepthSnapshot},
};
use pg_types::Venue;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

pub const SYMBOL: &str = "BTCUSDC";
pub const TRADE_STREAM: &str = "btcusdc@trade";
pub const BBO_STREAM: &str = "btcusdc@bookTicker";
pub const DEPTH_STREAM: &str = "btcusdc@depth@100ms";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Malformed,
    WrongStream,
    WrongSymbol,
    InvalidPriceOrSize,
    InvalidTimestamp,
    InvalidSequence,
}

#[derive(Deserialize)]
struct TradeWire {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "T")]
    trade_time_ms: u64,
    p: String,
    q: String,
    m: bool,
}

#[derive(Deserialize)]
struct BboWire {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "E")]
    event_time_ms: u64,
    b: String,
    #[serde(rename = "B")]
    bid_quantity: String,
    a: String,
    #[serde(rename = "A")]
    ask_quantity: String,
}

fn payload(bytes: &[u8], expected_stream: &str) -> Result<Value, WireError> {
    // Avoid unbounded JSON allocations from an untrusted public feed.
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        return Err(WireError::Malformed);
    }
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)?;
    if let Some(data) = envelope.get("data") {
        if envelope.get("stream").and_then(Value::as_str) != Some(expected_stream) {
            return Err(WireError::WrongStream);
        }
        return Ok(data.clone());
    }
    if !envelope.is_object() {
        return Err(WireError::Malformed);
    }
    Ok(envelope)
}

fn time_ns(ms: u64) -> Result<u64, WireError> {
    if ms == 0 {
        return Err(WireError::InvalidTimestamp);
    }
    ms.checked_mul(1_000_000)
        .ok_or(WireError::InvalidTimestamp)
}

fn decimal(value: &str, allow_zero: bool) -> Result<Decimal, WireError> {
    let decimal: Decimal = value.parse().map_err(|_| WireError::InvalidPriceOrSize)?;
    if decimal < Decimal::ZERO || (!allow_zero && decimal == Decimal::ZERO) {
        return Err(WireError::InvalidPriceOrSize);
    }
    Ok(decimal)
}

/// Convert one Binance trade event to a venue-neutral event. `m=true` means
/// buyer is maker, therefore SELL is the aggressor. Trade IDs are not assumed
/// to be contiguous and `sequence` remains None.
pub fn decode_trade(bytes: &[u8], received_ns: u64) -> Result<MarketEvent, WireError> {
    if received_ns == 0 {
        return Err(WireError::InvalidTimestamp);
    }
    let wire: TradeWire = serde_json::from_value(payload(bytes, TRADE_STREAM)?)
        .map_err(|_| WireError::Malformed)?;
    if wire.event_type != "trade" || wire.symbol != SYMBOL {
        return Err(WireError::WrongSymbol);
    }
    Ok(MarketEvent::Trade(TradeTick {
        venue: Venue::BinancePm,
        asset: SYMBOL.to_owned(),
        ts_event_ns: time_ns(wire.trade_time_ms)?,
        ts_recv_ns: received_ns,
        price: decimal(&wire.p, false)?,
        quantity: decimal(&wire.q, false)?,
        aggressor: if wire.m {
            AggressorSide::Sell
        } else {
            AggressorSide::Buy
        },
        sequence: None,
    }))
}

/// bookTicker `u` is not used as contiguous gap evidence: independent BBO
/// events need not increment by exactly one. L2 gaps use `U/u/pu` separately.
pub fn decode_bbo(bytes: &[u8], received_ns: u64) -> Result<MarketEvent, WireError> {
    if received_ns == 0 {
        return Err(WireError::InvalidTimestamp);
    }
    let wire: BboWire = serde_json::from_value(payload(bytes, BBO_STREAM)?)
        .map_err(|_| WireError::Malformed)?;
    if wire.event_type != "bookTicker" || wire.symbol != SYMBOL {
        return Err(WireError::WrongSymbol);
    }
    let bid_price = decimal(&wire.b, false)?;
    let ask_price = decimal(&wire.a, false)?;
    if bid_price >= ask_price {
        return Err(WireError::InvalidPriceOrSize);
    }
    Ok(MarketEvent::BestBidAsk(BestBidAsk {
        venue: Venue::BinancePm,
        asset: SYMBOL.to_owned(),
        ts_event_ns: time_ns(wire.event_time_ms)?,
        ts_recv_ns: received_ns,
        bid_price,
        bid_quantity: decimal(&wire.bid_quantity, true)?,
        ask_price,
        ask_quantity: decimal(&wire.ask_quantity, true)?,
        sequence: None,
    }))
}

pub fn decode_depth_delta(bytes: &[u8]) -> Result<BinanceDepthDelta, WireError> {
    let data = payload(bytes, DEPTH_STREAM)?;
    if data.get("e").and_then(Value::as_str) != Some("depthUpdate") {
        return Err(WireError::Malformed);
    }
    let wire: BinanceDepthDelta = serde_json::from_value(data).map_err(|_| WireError::Malformed)?;
    if wire.symbol != SYMBOL {
        return Err(WireError::WrongSymbol);
    }
    time_ns(wire.event_time_ms)?;
    if wire.first_update_id == 0 || wire.first_update_id > wire.final_update_id {
        return Err(WireError::InvalidSequence);
    }
    Ok(wire)
}

/// Parse an explicitly BTCUSDC-scoped REST snapshot response. The transport
/// must also check HTTP status/content type and the requested symbol.
pub fn decode_depth_snapshot(
    requested_symbol: &str,
    bytes: &[u8],
) -> Result<BinanceDepthSnapshot, WireError> {
    if requested_symbol != SYMBOL || bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return Err(WireError::WrongSymbol);
    }
    let snapshot: BinanceDepthSnapshot =
        serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)?;
    if snapshot.last_update_id == 0 {
        return Err(WireError::InvalidSequence);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_marketdata::binance_depth::BinanceDepthBridge;

    const SNAPSHOT: &[u8] = br#"{"lastUpdateId":100,"bids":[["99","2"]],"asks":[["101","3"]]}"#;
    const DEPTH: &[u8] = br#"{"e":"depthUpdate","E":1700000000000,"s":"BTCUSDC","U":99,"u":100,"pu":98,"b":[["100","1"]],"a":[]}"#;

    #[test]
    fn raw_and_combined_depth_match_and_bridge_inclusively() {
        let raw = decode_depth_delta(DEPTH).unwrap();
        let combined = format!(
            "{{\"stream\":\"btcusdc@depth@100ms\",\"data\":{}}}",
            String::from_utf8_lossy(DEPTH)
        );
        let wrapped = decode_depth_delta(combined.as_bytes()).unwrap();
        assert_eq!(raw.final_update_id, wrapped.final_update_id);
        let mut bridge = BinanceDepthBridge::new(SYMBOL);
        assert!(bridge.push(raw, 1).unwrap().is_none());
        let snapshot = decode_depth_snapshot(SYMBOL, SNAPSHOT).unwrap();
        assert_eq!(bridge.install_snapshot(snapshot).unwrap().unwrap().sequence, Some(100));
        assert!(bridge.is_ready());
    }

    #[test]
    fn wrong_stream_symbol_and_overlong_input_are_rejected() {
        assert_eq!(decode_depth_snapshot("BTCUSDT", SNAPSHOT).err(), Some(WireError::WrongSymbol));
        assert_eq!(decode_depth_delta(&vec![b'X'; 65537]).err(), Some(WireError::Malformed));
        let wrong = br#"{"stream":"btcusdt@depth@100ms","data":{"e":"depthUpdate","E":1,"s":"BTCUSDC","U":1,"u":2,"pu":0,"b":[],"a":[]}}"#;
        assert_eq!(decode_depth_delta(wrong).err(), Some(WireError::WrongStream));
    }

    #[test]
    fn trade_aggressor_and_bbo_are_normalized_without_fake_sequence() {
        let sell = br#"{"e":"trade","s":"BTCUSDC","T":1700000000000,"p":"80000.1","q":"0.002","m":true}"#;
        let buy = br#"{"e":"trade","s":"BTCUSDC","T":1700000000000,"p":"80000.1","q":"0.002","m":false}"#;
        match decode_trade(sell, 5).unwrap() {
            MarketEvent::Trade(t) => {
                assert_eq!(t.aggressor, AggressorSide::Sell);
                assert_eq!(t.sequence, None);
                assert_eq!(t.ts_recv_ns, 5);
            }
            _ => panic!("expected trade"),
        }
        match decode_trade(buy, 6).unwrap() {
            MarketEvent::Trade(t) => assert_eq!(t.aggressor, AggressorSide::Buy),
            _ => panic!("expected trade"),
        }
        let bbo = br#"{"e":"bookTicker","E":1700000000000,"s":"BTCUSDC","b":"80000","B":"1","a":"80000.1","A":"2"}"#;
        match decode_bbo(bbo, 7).unwrap() {
            MarketEvent::BestBidAsk(t) => {
                assert_eq!(t.sequence, None);
                assert!(t.bid_price < t.ask_price);
            }
            _ => panic!("expected BBO"),
        }
    }

    #[test]
    fn crossed_bbo_and_missing_timestamp_cannot_publish() {
        let crossed = br#"{"e":"bookTicker","E":1700000000000,"s":"BTCUSDC","b":"80001","B":"1","a":"80000","A":"2"}"#;
        assert_eq!(decode_bbo(crossed, 7).err(), Some(WireError::InvalidPriceOrSize));
        assert_eq!(decode_trade(br#"{}"#, 0).err(), Some(WireError::InvalidTimestamp));
    }
}
