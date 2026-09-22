//! Read-only USD-M market discovery, independent of Portfolio Margin credentials.
//! This deliberately does NOT register symbols for live order execution.

use std::{collections::BTreeSet, time::{Duration, SystemTime, UNIX_EPOCH}};

use pg_marketdata::{Candle, MarketDataError};
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::market_candles::{REST_KLINES, decode_rest_closed, interval_name, ws_stream};

pub const REST_EXCHANGE_INFO: &str = "https://fapi.binance.com/fapi/v1/exchangeInfo";
const MAX_EXCHANGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_KLINES_BYTES: usize = 2 * 1024 * 1024;

fn fail(reason: &str) -> MarketDataError {
    MarketDataError::Conversion(format!("invalid USD-M exchange information: {reason}"))
}

/// Returns sorted, unique, currently trading USDT/USDC-quoted USD-M perpetual
/// contracts. A Spot payload without `contractType=PERPETUAL` is NOT a catalog.
pub fn decode_trading_perpetuals(bytes: &[u8]) -> Result<BTreeSet<String>, MarketDataError> {
    if bytes.is_empty() || bytes.len() > MAX_EXCHANGE_BYTES {
        return Err(fail("empty or oversized exchangeInfo"));
    }
    let payload: Value = serde_json::from_slice(bytes).map_err(|_| fail("invalid JSON"))?;
    let rows = payload.get("symbols").and_then(Value::as_array)
        .filter(|rows| !rows.is_empty() && rows.len() <= 5000)
        .ok_or_else(|| fail("missing or excessive symbols"))?;
    let mut symbols = BTreeSet::new();
    for row in rows {
        if row.get("status").and_then(Value::as_str) != Some("TRADING")
            || row.get("contractType").and_then(Value::as_str) != Some("PERPETUAL")
        {
            continue;
        }
        let quote = row.get("quoteAsset").and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual quote missing"))?;
        if !matches!(quote, "USDT" | "USDC") {
            continue;
        }
        let symbol = row.get("symbol").and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual symbol missing"))?;
        let base = row.get("baseAsset").and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual base asset missing"))?;
        if symbol.len() > 32 || symbol.is_empty() || base.is_empty()
            || !symbol.bytes().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
            || symbol != format!("{base}{quote}")
        {
            return Err(fail("perpetual contract identity mismatch"));
        }
        if !symbols.insert(symbol.to_owned()) {
            return Err(fail("duplicate perpetual symbol"));
        }
    }
    if symbols.is_empty() {
        return Err(fail("no verified tradable USD-M perpetual symbols"));
    }
    Ok(symbols)
}

/// A separate research-only catalog. All endpoints are keyless HTTPS GETs;
/// the caller must pace symbol-specific requests within Binance IP weights.
pub struct PublicUsdMUniverse {
    http: Client,
    symbols: BTreeSet<String>,
}

impl PublicUsdMUniverse {
    pub fn new() -> Result<Self, MarketDataError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| MarketDataError::Disconnected("public REST client unavailable".into()))?;
        Ok(Self { http, symbols: BTreeSet::new() })
    }

    pub fn install_exchange_info(&mut self, bytes: &[u8]) -> Result<usize, MarketDataError> {
        // Atomic catalog replacement: preserve last known verified set on error.
        let next = decode_trading_perpetuals(bytes)?;
        let len = next.len();
        self.symbols = next;
        Ok(len)
    }

    pub async fn refresh(&mut self) -> Result<usize, MarketDataError> {
        let response = self.http.get(REST_EXCHANGE_INFO).send().await
            .map_err(|_| MarketDataError::Disconnected("public exchangeInfo request failed".into()))?;
        if response.status() != StatusCode::OK
            || response.content_length().is_some_and(|size| size > MAX_EXCHANGE_BYTES as u64)
        {
            return Err(MarketDataError::Disconnected("public exchangeInfo unavailable".into()));
        }
        let bytes = response.bytes().await
            .map_err(|_| MarketDataError::Disconnected("public exchangeInfo body incomplete".into()))?;
        self.install_exchange_info(&bytes)
    }

    pub fn symbols(&self) -> &BTreeSet<String> {
        &self.symbols
    }

    /// WS subscription name only. Does not connect or auto-subscribe the whole
    /// market: consumers must use bounded socket counts and rate limits.
    pub fn kline_stream(&self, symbol: &str, interval_ns: u64) -> Result<String, MarketDataError> {
        if !self.symbols.contains(symbol) {
            return Err(MarketDataError::Subscription("symbol not verified by exchangeInfo".into()));
        }
        ws_stream(symbol, interval_ns)
    }

    pub async fn closed_klines(&self, symbol: &str, interval_ns: u64, limit: u16)
        -> Result<Vec<Candle>, MarketDataError>
    {
        self.kline_stream(symbol, interval_ns)?;
        if !(2..=1500).contains(&limit) {
            return Err(MarketDataError::Subscription("invalid public kline limit".into()));
        }
        let interval = interval_name(interval_ns)
            .ok_or_else(|| MarketDataError::Subscription("unsupported interval".into()))?;
        let response = self.http.get(REST_KLINES)
            .query(&[("symbol", symbol), ("interval", interval), ("limit", &limit.to_string())])
            .send().await
            .map_err(|_| MarketDataError::Disconnected("public Klines request failed".into()))?;
        if response.status() != StatusCode::OK
            || response.content_length().is_some_and(|size| size > MAX_KLINES_BYTES as u64)
        {
            return Err(MarketDataError::Disconnected("public Klines unavailable".into()));
        }
        let bytes = response.bytes().await
            .map_err(|_| MarketDataError::Disconnected("public Klines body incomplete".into()))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| MarketDataError::Conversion("invalid host clock".into()))?;
        let received_ns = u64::try_from(now.as_nanos())
            .map_err(|_| MarketDataError::Conversion("host clock overflow".into()))?;
        decode_rest_closed(&bytes, symbol, interval_ns, received_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Vec<u8> {
        serde_json::to_vec(&json!({"symbols":[
            {"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","contractType":"PERPETUAL","status":"TRADING"},
            {"symbol":"ETHUSDT","baseAsset":"ETH","quoteAsset":"USDT","contractType":"PERPETUAL","status":"TRADING"},
            {"symbol":"DELISTUSDT","baseAsset":"DELIST","quoteAsset":"USDT","contractType":"PERPETUAL","status":"CLOSE"},
            {"symbol":"BTCUSDT_260925","baseAsset":"BTC","quoteAsset":"USDT","contractType":"CURRENT_QUARTER","status":"TRADING"}
        ]})).unwrap()
    }

    #[test]
    fn catalog_excludes_delisted_and_dated_contracts() {
        let universe = decode_trading_perpetuals(&sample()).unwrap();
        assert_eq!(universe.len(), 2);
        assert!(universe.contains("BTCUSDC"));
        assert!(universe.contains("ETHUSDT"));
        assert!(!universe.contains("DELISTUSDT"));
    }

    #[test]
    fn only_verified_symbols_get_public_kline_streams() {
        let mut universe = PublicUsdMUniverse::new().unwrap();
        assert!(universe.kline_stream("BTCUSDC", 60_000_000_000).is_err());
        universe.install_exchange_info(&sample()).unwrap();
        assert_eq!(universe.kline_stream("ETHUSDT", 60_000_000_000).unwrap(), "ethusdt@kline_1m");
        assert!(universe.kline_stream("BTCUSDT", 60_000_000_000).is_err());
        assert!(universe.kline_stream("ETHUSDT", 5_000_000_000).is_err());
    }

    #[test]
    fn malformed_spot_or_duplicate_catalog_cannot_replace_verified_symbols() {
        let mut universe = PublicUsdMUniverse::new().unwrap();
        universe.install_exchange_info(&sample()).unwrap();
        assert!(universe.install_exchange_info(&serde_json::to_vec(&json!({"symbols":[
            {"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","status":"TRADING"}
        ]})).unwrap()).is_err());
        assert_eq!(universe.symbols().len(), 2);
        let duplicate = serde_json::to_vec(&json!({"symbols":[
            {"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","contractType":"PERPETUAL","status":"TRADING"},
            {"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","contractType":"PERPETUAL","status":"TRADING"}
        ]})).unwrap();
        assert!(universe.install_exchange_info(&duplicate).is_err());
        assert_eq!(universe.symbols().len(), 2);
    }
}
