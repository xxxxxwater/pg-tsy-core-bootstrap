//! BTCUSDC public USD-M market transport. Portfolio Margin credentials belong
//! to private user/order APIs, never to the public candle/market-data feed.
//! A restarted candle subscription REST-verifies its last closed bar before
//! publishing new WS bars; historical REST bars are NEVER live entry signals.
//! The daemon owns bounded reconnect/backoff; no market data authorizes orders.

use std::{collections::BTreeMap, time::{Duration, SystemTime, UNIX_EPOCH}};

use async_trait::async_trait;
use futures_util::StreamExt;
use pg_marketdata::{
    Candle, FeedKind, FeedSpec, MarketDataError, MarketDataSource, MarketEvent,
    subscription::binance_depth::BinanceDepthBridge,
};
use pg_types::Venue;
use reqwest::{Client, StatusCode};
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::{
    market_candles::{CandleContinuity, REST_KLINES, decode_rest_closed, decode_ws_closed, interval_name, ws_stream},
    market_protocol::{
        BBO_STREAM, DEPTH_STREAM, SYMBOL, TRADE_STREAM, decode_bbo, decode_depth_delta,
        decode_depth_snapshot, decode_trade,
    },
};

const PUBLIC_WS: &str = "wss://fstream.binance.com/public/ws";
const CANDLE_WS: &str = "wss://fstream.binance.com/market/ws";
const DEPTH_REST: &str = "https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDC&limit=1000";
const MAX_WS_FRAME: usize = 64 * 1024;
const MAX_REST_FRAME: usize = 2 * 1024 * 1024;
type MarketSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn timestamp_ns() -> Result<u64, MarketDataError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MarketDataError::Conversion("invalid system clock".into()))?;
    u64::try_from(elapsed.as_nanos())
        .map_err(|_| MarketDataError::Conversion("clock overflow".into()))
}

/// Selecting an unsupported contract or feed fails before opening a socket.
/// This production-facing source is deliberately still scoped to the actual
/// USD-M BTCUSDC instrument; arbitrary Spot prices cannot impersonate it.
pub fn public_stream(spec: &FeedSpec) -> Result<String, MarketDataError> {
    if spec.venue != Venue::BinancePm || spec.asset != SYMBOL {
        return Err(MarketDataError::Subscription(
            "only Binance PM USD-M BTCUSDC is supported".into(),
        ));
    }
    match &spec.kind {
        FeedKind::Trades => Ok(TRADE_STREAM.into()),
        FeedKind::BestBidAsk => Ok(BBO_STREAM.into()),
        FeedKind::L2Book => Ok(DEPTH_STREAM.into()),
        FeedKind::Candle { interval_ns } => ws_stream(SYMBOL, *interval_ns),
    }
}

pub struct BinanceMarketDataSource {
    http: Client,
    last_closed: BTreeMap<u64, u64>,
}

impl BinanceMarketDataSource {
    pub fn new() -> Result<Self, MarketDataError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| MarketDataError::Disconnected("cannot initialize TLS REST".into()))?;
        Ok(Self {
            http,
            last_closed: BTreeMap::new(),
        })
    }

    async fn depth_snapshot(
        &self,
    ) -> Result<pg_marketdata::subscription::binance_depth::BinanceDepthSnapshot, MarketDataError>
    {
        let response = self.http.get(DEPTH_REST).send().await.map_err(|_| {
            MarketDataError::Disconnected("depth snapshot REST transport failed".into())
        })?;
        if response.status() != StatusCode::OK
            || response.content_length().is_some_and(|n| n > MAX_REST_FRAME as u64)
        {
            return Err(MarketDataError::Disconnected(
                "depth snapshot unavailable or oversized".into(),
            ));
        }
        let bytes = response.bytes().await.map_err(|_| {
            MarketDataError::Disconnected("depth snapshot response incomplete".into())
        })?;
        decode_depth_snapshot(SYMBOL, &bytes)
            .map_err(|err| MarketDataError::Conversion(format!("invalid depth snapshot: {err:?}")))
    }

    /// Read-only, keyless USD-M REST candles. This is historical evidence, not
    /// a MarketEvent subscription and never directly triggers an entry signal.
    /// A full-market scanner must first validate symbols with USD-M exchangeInfo;
    /// this PM strategy adapter cannot silently substitute a Spot instrument.
    pub async fn fetch_recent_closed(
        &self,
        spec: &FeedSpec,
        limit: u16,
    ) -> Result<Vec<Candle>, MarketDataError> {
        public_stream(spec)?;
        let FeedKind::Candle { interval_ns } = &spec.kind else {
            return Err(MarketDataError::Subscription("REST backfill requires candle feed".into()));
        };
        if !(2..=1500).contains(&limit) {
            return Err(MarketDataError::Subscription("REST kline limit must be 2..=1500".into()));
        }
        let interval = interval_name(*interval_ns)
            .ok_or_else(|| MarketDataError::Subscription("unsupported candle interval".into()))?;
        let response = self.http.get(REST_KLINES)
            .query(&[("symbol", SYMBOL), ("interval", interval), ("limit", &limit.to_string())])
            .send().await
            .map_err(|_| MarketDataError::Disconnected("public kline REST request failed".into()))?;
        if response.status() != StatusCode::OK
            || response.content_length().is_some_and(|n| n > MAX_REST_FRAME as u64)
        {
            return Err(MarketDataError::Disconnected("public kline REST unavailable or oversized".into()));
        }
        let bytes = response.bytes().await
            .map_err(|_| MarketDataError::Disconnected("public kline REST response incomplete".into()))?;
        decode_rest_closed(&bytes, SYMBOL, *interval_ns, timestamp_ns()?)
    }

    async fn publish(
        sink: &mpsc::Sender<MarketEvent>,
        event: MarketEvent,
    ) -> Result<(), MarketDataError> {
        sink.send(event)
            .await
            .map_err(|_| MarketDataError::Disconnected("market-data consumer closed".into()))
    }

    async fn stream_simple(
        &self,
        socket: &mut MarketSocket,
        kind: &FeedKind,
        sink: &mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        loop {
            let payload = next_json(socket).await?;
            let received_ns = timestamp_ns()?;
            let event = match kind {
                FeedKind::Trades => decode_trade(&payload, received_ns),
                FeedKind::BestBidAsk => decode_bbo(&payload, received_ns),
                _ => {
                    return Err(MarketDataError::Subscription(
                        "unsupported Binance feed".into(),
                    ));
                }
            }
            .map_err(|err| MarketDataError::Conversion(format!("invalid Binance feed: {err:?}")))?;
            Self::publish(sink, event).await?;
        }
    }

    async fn stream_candles(
        &mut self,
        socket: &mut MarketSocket,
        interval_ns: u64,
        sink: &mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        let spec = FeedSpec {
            venue: Venue::BinancePm,
            asset: SYMBOL.into(),
            kind: FeedKind::Candle { interval_ns },
        };
        // WS is already connected, so any frames arriving during this REST
        // request are buffered by the socket and checked against the seed.
        let recent = self.fetch_recent_closed(&spec, 3).await?;
        let latest = recent.last().ok_or_else(|| {
            MarketDataError::Disconnected("no closed USD-M candle to establish REST baseline".into())
        })?;
        if let Some(previous) = self.last_closed.get(&interval_ns).copied()
            && previous != latest.start_ns
        {
            return Err(MarketDataError::Disconnected(
                "closed candles changed during disconnect; explicit historical replay required".into(),
            ));
        }
        let mut cursor = CandleContinuity::default();
        cursor.accept(latest)?;
        self.last_closed.insert(interval_ns, latest.start_ns);
        // Never send historical REST bars into the live signal path: that
        // would turn a stale candle into a new actionable strategy decision.
        loop {
            let payload = next_json(socket).await?;
            let Some(candle) = decode_ws_closed(&payload, SYMBOL, interval_ns, timestamp_ns()?)?
            else {
                continue;
            };
            if cursor.accept(&candle)? {
                self.last_closed.insert(interval_ns, candle.start_ns);
                Self::publish(sink, MarketEvent::Candle(candle)).await?;
            }
        }
    }

    async fn stream_depth(
        &self,
        socket: &mut MarketSocket,
        sink: &mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        // First buffer an actual WS diff before fetching the REST snapshot.
        // This avoids a snapshot racing ahead of the earliest WS diff.
        let mut bridge = BinanceDepthBridge::new(SYMBOL);
        let first = next_json(socket).await?;
        let delta = decode_depth_delta(&first)
            .map_err(|err| MarketDataError::Conversion(format!("invalid depth diff: {err:?}")))?;
        bridge
            .push(delta, timestamp_ns()?)
            .map_err(|err| MarketDataError::Conversion(err.to_string()))?;

        let snapshot_task = self.depth_snapshot();
        tokio::pin!(snapshot_task);
        let mut snapshot_loaded = false;
        loop {
            tokio::select! {
                snapshot = &mut snapshot_task, if !snapshot_loaded => {
                    snapshot_loaded = true;
                    if let Some(book) = bridge.install_snapshot(snapshot?)
                        .map_err(|err| MarketDataError::Conversion(err.to_string()))?
                    {
                        Self::publish(sink, MarketEvent::L2Book(book)).await?;
                    }
                }
                payload = next_json(socket) => {
                    let delta = decode_depth_delta(&payload?)
                        .map_err(|err| MarketDataError::Conversion(format!("invalid depth diff: {err:?}")))?;
                    if let Some(book) = bridge.push(delta, timestamp_ns()?)
                        .map_err(|err| MarketDataError::Conversion(err.to_string()))?
                    {
                        // The bridge NEVER returns a publishable book before
                        // the REST snapshot has been successfully bridged.
                        Self::publish(sink, MarketEvent::L2Book(book)).await?;
                    }
                }
            }
        }
    }
}

/// One complete session. Disconnect, timeout or sequence gap returns an error;
/// the parent reconnect supervisor must open a fresh socket and resnapshot.
#[async_trait]
impl MarketDataSource for BinanceMarketDataSource {
    async fn stream(
        &mut self,
        spec: FeedSpec,
        sink: mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        let stream = public_stream(&spec)?;
        let base = if matches!(spec.kind, FeedKind::Candle { .. }) {
            CANDLE_WS
        } else {
            PUBLIC_WS
        };
        let url = format!("{base}/{stream}");
        let (mut socket, _) = timeout(Duration::from_secs(5), connect_async(&url))
            .await
            .map_err(|_| MarketDataError::Disconnected("public websocket connect timeout".into()))?
            .map_err(|_| MarketDataError::Disconnected("public websocket handshake failed".into()))?;
        match spec.kind {
            FeedKind::L2Book => self.stream_depth(&mut socket, &sink).await,
            FeedKind::Candle { interval_ns } => self.stream_candles(&mut socket, interval_ns, &sink).await,
            ref kind => self.stream_simple(&mut socket, kind, &sink).await,
        }
    }
}

async fn next_json(socket: &mut MarketSocket) -> Result<Vec<u8>, MarketDataError> {
    loop {
        let frame = timeout(Duration::from_secs(15), socket.next())
            .await
            .map_err(|_| MarketDataError::Disconnected("public websocket idle timeout".into()))?
            .ok_or_else(|| MarketDataError::Disconnected("public websocket EOF".into()))?
            .map_err(|_| MarketDataError::Disconnected("public websocket read failed".into()))?;
        match frame {
            Message::Text(text) => {
                if text.is_empty() || text.len() > MAX_WS_FRAME {
                    return Err(MarketDataError::Conversion(
                        "invalid websocket frame size".into(),
                    ));
                }
                return Ok(text.as_bytes().to_vec());
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => {
                return Err(MarketDataError::Disconnected(
                    "public websocket closed".into(),
                ));
            }
            _ => {
                return Err(MarketDataError::Conversion(
                    "non-text market message".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(asset: &str, venue: Venue, kind: FeedKind) -> FeedSpec {
        FeedSpec {
            venue,
            asset: asset.into(),
            kind,
        }
    }

    #[test]
    fn btcusdc_public_stream_selection_is_explicit_and_restricted() {
        assert_eq!(
            public_stream(&spec(SYMBOL, Venue::BinancePm, FeedKind::Trades)).unwrap(),
            TRADE_STREAM
        );
        assert_eq!(
            public_stream(&spec(SYMBOL, Venue::BinancePm, FeedKind::BestBidAsk)).unwrap(),
            BBO_STREAM
        );
        assert_eq!(
            public_stream(&spec(SYMBOL, Venue::BinancePm, FeedKind::L2Book)).unwrap(),
            DEPTH_STREAM
        );
        assert!(public_stream(&spec("BTCUSDT", Venue::BinancePm, FeedKind::Trades)).is_err());
        assert!(public_stream(&spec(SYMBOL, Venue::Hyperliquid, FeedKind::Trades)).is_err());
        assert_eq!(
            public_stream(&spec(SYMBOL, Venue::BinancePm, FeedKind::Candle {
                interval_ns: 60_000_000_000
            })).unwrap(),
            "btcusdc@kline_1m"
        );
        assert!(public_stream(&spec(SYMBOL, Venue::BinancePm, FeedKind::Candle {
            interval_ns: 5_000_000_000
        })).is_err());
    }

    #[test]
    fn client_construction_does_not_start_network_or_trading() {
        assert!(BinanceMarketDataSource::new().is_ok());
    }
}
