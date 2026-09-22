//! Public Binance USD-M candles are market data, not Portfolio Margin account data.
//! REST provides bounded closed-candle backfill; WS provides finalised bars only.
//! Never feed Spot/COIN-M prices into a USD-M instrument under the PM venue.

use pg_marketdata::{Candle, MarketDataError};
use pg_types::Venue;
use rust_decimal::Decimal;
use serde_json::Value;

pub const REST_KLINES: &str = "https://fapi.binance.com/fapi/v1/klines";
const MAX_REST_BYTES: usize = 2 * 1024 * 1024;
const MAX_WS_BYTES: usize = 64 * 1024;
const MS_TO_NS: u64 = 1_000_000;

pub fn interval_name(interval_ns: u64) -> Option<&'static str> {
    match interval_ns {
        60_000_000_000 => Some("1m"),
        180_000_000_000 => Some("3m"),
        300_000_000_000 => Some("5m"),
        900_000_000_000 => Some("15m"),
        1_800_000_000_000 => Some("30m"),
        3_600_000_000_000 => Some("1h"),
        14_400_000_000_000 => Some("4h"),
        86_400_000_000_000 => Some("1d"),
        _ => None,
    }
}

pub fn ws_stream(symbol: &str, interval_ns: u64) -> Result<String, MarketDataError> {
    if symbol.is_empty()
        || symbol.len() > 32
        || !symbol
            .bytes()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
    {
        return Err(MarketDataError::Subscription(
            "invalid USD-M market symbol".into(),
        ));
    }
    let interval = interval_name(interval_ns)
        .ok_or_else(|| MarketDataError::Subscription("unsupported USD-M candle interval".into()))?;
    Ok(format!("{}@kline_{interval}", symbol.to_ascii_lowercase()))
}

fn invalid(reason: &str) -> MarketDataError {
    MarketDataError::Conversion(format!("invalid public USD-M candle: {reason}"))
}

fn field<'a>(row: &'a Value, name: &str) -> Result<&'a str, MarketDataError> {
    row.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing string field"))
}

fn decimal(value: &str) -> Result<Decimal, MarketDataError> {
    let value: Decimal = value.parse().map_err(|_| invalid("invalid decimal"))?;
    if value < Decimal::ZERO {
        return Err(invalid("negative decimal"));
    }
    Ok(value)
}

fn ms_ns(value: u64) -> Result<u64, MarketDataError> {
    value
        .checked_mul(MS_TO_NS)
        .ok_or_else(|| invalid("timestamp overflow"))
}

#[allow(clippy::too_many_arguments)]
fn validated_candle(
    symbol: &str,
    interval_ns: u64,
    start_ms: u64,
    close_ms: u64,
    recv_ns: u64,
    open: &str,
    high: &str,
    low: &str,
    close: &str,
    volume: &str,
    trades: u64,
) -> Result<Candle, MarketDataError> {
    let interval_ms = interval_ns / MS_TO_NS;
    if interval_ms == 0
        || start_ms == 0
        || start_ms % interval_ms != 0
        || start_ms
            .checked_add(interval_ms)
            .and_then(|t| t.checked_sub(1))
            != Some(close_ms)
        || recv_ns == 0
    {
        return Err(invalid("misaligned or inconsistent candle timestamps"));
    }
    let (open, high, low, close, volume) = (
        decimal(open)?,
        decimal(high)?,
        decimal(low)?,
        decimal(close)?,
        decimal(volume)?,
    );
    if low <= Decimal::ZERO
        || open < low
        || open > high
        || close < low
        || close > high
        || high < low
    {
        return Err(invalid("inconsistent OHLC"));
    }
    Ok(Candle {
        venue: Venue::BinancePm,
        asset: symbol.into(),
        interval_ns,
        start_ns: ms_ns(start_ms)?,
        end_ns: ms_ns(
            start_ms
                .checked_add(interval_ms)
                .ok_or_else(|| invalid("end overflow"))?,
        )?,
        ts_recv_ns: recv_ns,
        open,
        high,
        low,
        close,
        volume,
        trades,
    })
}

/// Validate both stream and symbol (including the nested symbol), and never
/// publish an incomplete candle. A caller must still enforce continuity.
pub fn decode_ws_closed(
    bytes: &[u8],
    expected_symbol: &str,
    interval_ns: u64,
    received_ns: u64,
) -> Result<Option<Candle>, MarketDataError> {
    if bytes.is_empty() || bytes.len() > MAX_WS_BYTES {
        return Err(invalid("oversized or empty websocket event"));
    }
    let expected_stream = ws_stream(expected_symbol, interval_ns)?;
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| invalid("malformed JSON"))?;
    let data = if let Some(data) = envelope.get("data") {
        if envelope.get("stream").and_then(Value::as_str) != Some(expected_stream.as_str()) {
            return Err(invalid("wrong combined stream"));
        }
        data
    } else {
        &envelope
    };
    if data.get("e").and_then(Value::as_str) != Some("kline")
        || field(data, "s")? != expected_symbol
        || data
            .get("E")
            .and_then(Value::as_u64)
            .filter(|ts| *ts > 0)
            .is_none()
        || data.get("st").is_some_and(|kind| kind.as_u64() != Some(1))
    {
        return Err(invalid("wrong event type, symbol or market"));
    }
    let k = data
        .get("k")
        .ok_or_else(|| invalid("missing kline payload"))?;
    if field(k, "s")? != expected_symbol
        || field(k, "i")? != interval_name(interval_ns).unwrap_or("")
    {
        return Err(invalid("nested symbol or interval mismatch"));
    }
    let candle = validated_candle(
        expected_symbol,
        interval_ns,
        k.get("t")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("start missing"))?,
        k.get("T")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("close missing"))?,
        received_ns,
        field(k, "o")?,
        field(k, "h")?,
        field(k, "l")?,
        field(k, "c")?,
        field(k, "v")?,
        k.get("n")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("trade count missing"))?,
    )?;
    match k.get("x").and_then(Value::as_bool) {
        Some(true) => Ok(Some(candle)),
        Some(false) => Ok(None),
        None => Err(invalid("missing close flag")),
    }
}

/// Decode /fapi/v1/klines (not /papi/v1 or Spot). The endpoint has no `x`
/// flag, so a bar is final only after its inclusive close timestamp passes.
/// Reject unordered, duplicate, contradictory and noncontiguous closed bars.
pub fn decode_rest_closed(
    bytes: &[u8],
    symbol: &str,
    interval_ns: u64,
    received_ns: u64,
) -> Result<Vec<Candle>, MarketDataError> {
    ws_stream(symbol, interval_ns)?;
    if bytes.is_empty() || bytes.len() > MAX_REST_BYTES {
        return Err(invalid("oversized or empty REST response"));
    }
    let rows: Value = serde_json::from_slice(bytes).map_err(|_| invalid("malformed REST JSON"))?;
    let rows = rows
        .as_array()
        .ok_or_else(|| invalid("REST response is not an array"))?;
    if rows.len() > 1500 {
        return Err(invalid("REST page exceeds limit"));
    }
    let mut candles: Vec<Candle> = Vec::with_capacity(rows.len());
    for row in rows {
        let values = row
            .as_array()
            .filter(|values| values.len() >= 12)
            .ok_or_else(|| invalid("malformed REST tuple"))?;
        let start = values[0]
            .as_u64()
            .ok_or_else(|| invalid("missing REST start"))?;
        let close = values[6]
            .as_u64()
            .ok_or_else(|| invalid("missing REST close"))?;
        let candle = validated_candle(
            symbol,
            interval_ns,
            start,
            close,
            received_ns,
            values[1].as_str().ok_or_else(|| invalid("open missing"))?,
            values[2].as_str().ok_or_else(|| invalid("high missing"))?,
            values[3].as_str().ok_or_else(|| invalid("low missing"))?,
            values[4]
                .as_str()
                .ok_or_else(|| invalid("close price missing"))?,
            values[5]
                .as_str()
                .ok_or_else(|| invalid("volume missing"))?,
            values[8]
                .as_u64()
                .ok_or_else(|| invalid("trades missing"))?,
        )?;
        if let Some(previous) = candles.last()
            && candle.start_ns != previous.end_ns
        {
            return Err(invalid("REST bar gap, overlap or reorder"));
        }
        // A REST reply may contain the currently forming bar. Never emit it.
        if candle.end_ns <= received_ns {
            candles.push(candle);
        } else {
            // Only the last row may be unfinished, and it is not a signal.
            if row != rows.last().expect("loop row exists") {
                return Err(invalid("nonterminal unfinished REST bar"));
            }
        }
    }
    Ok(candles)
}

/// Session-local monotonic guard. Every reconnect must REST-backfill before
/// trusting incoming websocket candles; this guard never invents missing bars.
#[derive(Default)]
pub struct CandleContinuity {
    previous_start_ns: Option<u64>,
}

impl CandleContinuity {
    pub fn accept(&mut self, candle: &Candle) -> Result<bool, MarketDataError> {
        if let Some(previous) = self.previous_start_ns {
            if candle.start_ns == previous {
                return Ok(false);
            }
            if previous.checked_add(candle.interval_ns) != Some(candle.start_ns) {
                return Err(MarketDataError::Disconnected(
                    "USD-M closed-kline gap or reorder; REST backfill required".into(),
                ));
            }
        }
        self.previous_start_ns = Some(candle.start_ns);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MINUTE: u64 = 60_000_000_000;
    const START: u64 = 1_700_000_040_000;

    fn ws(closed: bool) -> Vec<u8> {
        serde_json::to_vec(
            &json!({"e":"kline", "E":START + 60_000, "s":"BTCUSDC", "st":1,
            "k":{"t":START, "T":START + 59_999, "s":"BTCUSDC", "i":"1m",
                "o":"10", "h":"12", "l":"9", "c":"11", "v":"2", "n":4, "x":closed}}),
        )
        .unwrap()
    }

    #[test]
    fn ws_requires_finalised_exact_usdm_contract() {
        assert!(
            decode_ws_closed(&ws(false), "BTCUSDC", MINUTE, 10)
                .unwrap()
                .is_none()
        );
        let candle = decode_ws_closed(&ws(true), "BTCUSDC", MINUTE, 10)
            .unwrap()
            .unwrap();
        assert_eq!(candle.end_ns - candle.start_ns, MINUTE);
        assert_eq!(candle.close, Decimal::from(11));
        assert!(decode_ws_closed(&ws(true), "BTCUSDT", MINUTE, 10).is_err());
        assert!(decode_ws_closed(&ws(true), "BTCUSDC", 5_000_000_000, 10).is_err());
        let combined = json!({"stream":"btcusdc@kline_1m", "data":serde_json::from_slice::<Value>(&ws(true)).unwrap()});
        assert!(
            decode_ws_closed(
                &serde_json::to_vec(&combined).unwrap(),
                "BTCUSDC",
                MINUTE,
                10
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn rest_skips_live_bar_and_detects_missing_bar() {
        let row = |start: u64| {
            json!([
                start,
                "10",
                "12",
                "9",
                "11",
                "2",
                start + 59_999,
                "22",
                4,
                "1",
                "11",
                "0"
            ])
        };
        let data = serde_json::to_vec(&json!([row(START), row(START + 60_000)])).unwrap();
        let closed =
            decode_rest_closed(&data, "BTCUSDC", MINUTE, (START + 60_000) * MS_TO_NS).unwrap();
        assert_eq!(closed.len(), 1);
        let gap = serde_json::to_vec(&json!([row(START), row(START + 120_000)])).unwrap();
        assert!(decode_rest_closed(&gap, "BTCUSDC", MINUTE, (START + 180_000) * MS_TO_NS).is_err());
    }

    #[test]
    fn continuity_accepts_one_bar_once_and_blocks_gaps() {
        let one = decode_ws_closed(&ws(true), "BTCUSDC", MINUTE, 10)
            .unwrap()
            .unwrap();
        let mut cursor = CandleContinuity::default();
        assert!(cursor.accept(&one).unwrap());
        assert!(!cursor.accept(&one).unwrap());
        let mut gap = one.clone();
        gap.start_ns += MINUTE * 2;
        gap.end_ns += MINUTE * 2;
        assert!(cursor.accept(&gap).is_err());
    }
}
