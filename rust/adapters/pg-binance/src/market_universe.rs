//! Read-only USD-M market discovery, independent of Portfolio Margin credentials.
//! This deliberately does NOT register symbols for live order execution.

use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pg_marketdata::{Candle, MarketDataError};
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::{
    market_candles::{REST_KLINES, decode_rest_closed, interval_name, ws_stream},
    order_protocol::SymbolFilters,
};

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
    let rows = payload
        .get("symbols")
        .and_then(Value::as_array)
        .filter(|rows| !rows.is_empty() && rows.len() <= 5000)
        .ok_or_else(|| fail("missing or excessive symbols"))?;
    let mut symbols = BTreeSet::new();
    for row in rows {
        if row.get("status").and_then(Value::as_str) != Some("TRADING")
            || row.get("contractType").and_then(Value::as_str) != Some("PERPETUAL")
        {
            continue;
        }
        let quote = row
            .get("quoteAsset")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual quote missing"))?;
        if !matches!(quote, "USDT" | "USDC") {
            continue;
        }
        let symbol = row
            .get("symbol")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual symbol missing"))?;
        let base = row
            .get("baseAsset")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("perpetual base asset missing"))?;
        if symbol.len() > 32
            || symbol.is_empty()
            || base.is_empty()
            || !symbol
                .bytes()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
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

fn filter<'a>(filters: &'a [Value], kind: &str) -> Result<&'a Value, MarketDataError> {
    let matches = filters
        .iter()
        .filter(|value| value.get("filterType").and_then(Value::as_str) == Some(kind))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(fail("missing or duplicate symbol filter"));
    }
    Ok(matches[0])
}

fn positive_filter_decimal(value: &Value, names: &[&str]) -> Result<String, MarketDataError> {
    let raw = names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .ok_or_else(|| fail("required filter field missing"))?;
    let parsed = raw
        .parse::<rust_decimal::Decimal>()
        .map_err(|_| fail("invalid decimal filter field"))?;
    if parsed <= rust_decimal::Decimal::ZERO {
        return Err(fail("non-positive decimal filter field"));
    }
    Ok(raw.to_owned())
}

/// Extract the exact executable BTCUSDC USD-M contract constraints. This
/// parser rejects duplicate symbols/filters and never substitutes precision
/// fields for authoritative tick/step filters.
pub fn decode_symbol_filters(
    bytes: &[u8],
    requested_symbol: &str,
) -> Result<SymbolFilters, MarketDataError> {
    if requested_symbol != crate::order_protocol::SYMBOL
        || bytes.is_empty()
        || bytes.len() > MAX_EXCHANGE_BYTES
    {
        return Err(fail("unsupported requested execution symbol"));
    }
    let payload: Value = serde_json::from_slice(bytes).map_err(|_| fail("invalid JSON"))?;
    let symbols = payload
        .get("symbols")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("symbols missing"))?;
    let matches = symbols
        .iter()
        .filter(|row| row.get("symbol").and_then(Value::as_str) == Some(requested_symbol))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(fail("execution symbol missing or duplicated"));
    }
    let row = matches[0];
    if row.get("status").and_then(Value::as_str) != Some("TRADING")
        || row.get("contractType").and_then(Value::as_str) != Some("PERPETUAL")
        || row.get("baseAsset").and_then(Value::as_str) != Some("BTC")
        || row.get("quoteAsset").and_then(Value::as_str) != Some("USDC")
    {
        return Err(fail("BTCUSDC contract identity or trading status mismatch"));
    }
    let filters = row
        .get("filters")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("symbol filters missing"))?;
    let price = filter(filters, "PRICE_FILTER")?;
    let lot = filter(filters, "LOT_SIZE")?;
    let notional = filters
        .iter()
        .filter(|value| {
            matches!(
                value.get("filterType").and_then(Value::as_str),
                Some("MIN_NOTIONAL" | "NOTIONAL")
            )
        })
        .collect::<Vec<_>>();
    if notional.len() != 1 {
        return Err(fail("missing or ambiguous notional filter"));
    }

    Ok(SymbolFilters {
        symbol: requested_symbol.to_owned(),
        tick_size: positive_filter_decimal(price, &["tickSize"])?,
        step_size: positive_filter_decimal(lot, &["stepSize"])?,
        min_quantity: positive_filter_decimal(lot, &["minQty"])?,
        min_notional: positive_filter_decimal(notional[0], &["notional", "minNotional"])?,
        symbol_trading: true,
    })
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
        Ok(Self {
            http,
            symbols: BTreeSet::new(),
        })
    }

    pub fn install_exchange_info(&mut self, bytes: &[u8]) -> Result<usize, MarketDataError> {
        // Atomic catalog replacement: preserve last known verified set on error.
        let next = decode_trading_perpetuals(bytes)?;
        let len = next.len();
        self.symbols = next;
        Ok(len)
    }

    pub async fn fetch_symbol_filters(
        &self,
        symbol: &str,
    ) -> Result<SymbolFilters, MarketDataError> {
        let response = self
            .http
            .get(REST_EXCHANGE_INFO)
            .send()
            .await
            .map_err(|_| {
                MarketDataError::Disconnected("public exchangeInfo request failed".into())
            })?;
        if response.status() != StatusCode::OK
            || response
                .content_length()
                .is_some_and(|size| size > MAX_EXCHANGE_BYTES as u64)
        {
            return Err(MarketDataError::Disconnected(
                "public exchangeInfo unavailable".into(),
            ));
        }
        let bytes = response.bytes().await.map_err(|_| {
            MarketDataError::Disconnected("public exchangeInfo body incomplete".into())
        })?;
        decode_symbol_filters(&bytes, symbol)
    }

    pub async fn refresh(&mut self) -> Result<usize, MarketDataError> {
        let response = self
            .http
            .get(REST_EXCHANGE_INFO)
            .send()
            .await
            .map_err(|_| {
                MarketDataError::Disconnected("public exchangeInfo request failed".into())
            })?;
        if response.status() != StatusCode::OK
            || response
                .content_length()
                .is_some_and(|size| size > MAX_EXCHANGE_BYTES as u64)
        {
            return Err(MarketDataError::Disconnected(
                "public exchangeInfo unavailable".into(),
            ));
        }
        let bytes = response.bytes().await.map_err(|_| {
            MarketDataError::Disconnected("public exchangeInfo body incomplete".into())
        })?;
        self.install_exchange_info(&bytes)
    }

    pub fn symbols(&self) -> &BTreeSet<String> {
        &self.symbols
    }

    /// WS subscription name only. Does not connect or auto-subscribe the whole
    /// market: consumers must use bounded socket counts and rate limits.
    pub fn kline_stream(&self, symbol: &str, interval_ns: u64) -> Result<String, MarketDataError> {
        if !self.symbols.contains(symbol) {
            return Err(MarketDataError::Subscription(
                "symbol not verified by exchangeInfo".into(),
            ));
        }
        ws_stream(symbol, interval_ns)
    }

    pub async fn closed_klines(
        &self,
        symbol: &str,
        interval_ns: u64,
        limit: u16,
    ) -> Result<Vec<Candle>, MarketDataError> {
        self.kline_stream(symbol, interval_ns)?;
        if !(2..=1500).contains(&limit) {
            return Err(MarketDataError::Subscription(
                "invalid public kline limit".into(),
            ));
        }
        let interval = interval_name(interval_ns)
            .ok_or_else(|| MarketDataError::Subscription("unsupported interval".into()))?;
        let response = self
            .http
            .get(REST_KLINES)
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .map_err(|_| MarketDataError::Disconnected("public Klines request failed".into()))?;
        if response.status() != StatusCode::OK
            || response
                .content_length()
                .is_some_and(|size| size > MAX_KLINES_BYTES as u64)
        {
            return Err(MarketDataError::Disconnected(
                "public Klines unavailable".into(),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| MarketDataError::Disconnected("public Klines body incomplete".into()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
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
    fn executable_filters_come_from_exact_exchange_info_filters() {
        let payload = serde_json::to_vec(&json!({"symbols":[{
            "symbol":"BTCUSDC",
            "baseAsset":"BTC",
            "quoteAsset":"USDC",
            "contractType":"PERPETUAL",
            "status":"TRADING",
            "filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.10","minPrice":"1","maxPrice":"1000000"},
                {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"100"},
                {"filterType":"MIN_NOTIONAL","notional":"5"}
            ]
        }]}))
        .unwrap();
        let filters = decode_symbol_filters(&payload, "BTCUSDC").unwrap();
        assert_eq!(filters.tick_size, "0.10");
        assert_eq!(filters.step_size, "0.001");
        assert_eq!(filters.min_quantity, "0.001");
        assert_eq!(filters.min_notional, "5");
        assert!(filters.symbol_trading);
    }

    #[test]
    fn execution_filters_reject_wrong_contract_or_ambiguous_notional() {
        let wrong = serde_json::to_vec(&json!({"symbols":[{
            "symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDT",
            "contractType":"PERPETUAL","status":"TRADING","filters":[]
        }]}))
        .unwrap();
        assert!(decode_symbol_filters(&wrong, "BTCUSDC").is_err());

        let duplicate = serde_json::to_vec(&json!({"symbols":[{
            "symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC",
            "contractType":"PERPETUAL","status":"TRADING",
            "filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.1"},
                {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001"},
                {"filterType":"MIN_NOTIONAL","notional":"5"},
                {"filterType":"NOTIONAL","minNotional":"5"}
            ]
        }]}))
        .unwrap();
        assert!(decode_symbol_filters(&duplicate, "BTCUSDC").is_err());
    }

    #[test]
    fn only_verified_symbols_get_public_kline_streams() {
        let mut universe = PublicUsdMUniverse::new().unwrap();
        assert!(universe.kline_stream("BTCUSDC", 60_000_000_000).is_err());
        universe.install_exchange_info(&sample()).unwrap();
        assert_eq!(
            universe.kline_stream("ETHUSDT", 60_000_000_000).unwrap(),
            "ethusdt@kline_1m"
        );
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
