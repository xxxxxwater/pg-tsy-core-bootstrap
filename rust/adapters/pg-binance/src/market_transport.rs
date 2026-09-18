//! BTCUSDC public USD-M market transport. The supervising daemon owns bounded
//! reconnect/backoff; each new websocket gets a *new* depth bridge and snapshot.
//! Public data NEVER enables order submission.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::StreamExt;
use pg_marketdata::{
    FeedKind, FeedSpec, MarketDataError, MarketDataSource, MarketEvent,
    subscription::binance_depth::BinanceDepthBridge,
};
use pg_types::Venue;
use reqwest::{Client, StatusCode};
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::market_protocol::{
    BBO_STREAM, DEPTH_STREAM, SYMBOL, TRADE_STREAM, decode_bbo, decode_depth_delta,
    decode_depth_snapshot, decode_trade,
};

const PUBLIC_WS: &str = "wss://fstream.binance.com/public/ws";
const DEPTH_REST: &str = "https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDC&limit=1000";
const MAX_WS_FRAME: usize = 64 * 1024;
type MarketSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn timestamp_ns() -> Result<u64, MarketDataError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MarketDataError::Conversion("invalid system clock".into()))?;
    u64::try_from(elapsed.as_nanos())
        .map_err(|_| MarketDataError::Conversion("clock overflow".into()))
}

/// Selecting an unsupported instrument or feed fails before opening a socket.
pub fn public_stream(spec: &FeedSpec) -> Result<&'static str, MarketDataError> {
    if spec.venue != Venue::BinancePm || spec.asset != SYMBOL {
        return Err(MarketDataError::Subscription(
            "only Binance PM BTCUSDC is supported".into(),
        ));
    }
    match &spec.kind {
        FeedKind::Trades => Ok(TRADE_STREAM),
        FeedKind::BestBidAsk => Ok(BBO_STREAM),
        FeedKind::L2Book => Ok(DEPTH_STREAM),
        FeedKind::Candle { .. } => Err(MarketDataError::Subscription(
            "Binance candle websocket transport is not implemented".into(),
        )),
    }
}

pub struct BinanceMarketDataSource {
    http: Client,
}

impl BinanceMarketDataSource {
    pub fn new() -> Result<Self, MarketDataError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| MarketDataError::Disconnected("cannot initialize TLS REST".into()))?;
        Ok(Self { http })
    }

    async fn depth_snapshot(
        &self,
    ) -> Result<pg_marketdata::subscription::binance_depth::BinanceDepthSnapshot, MarketDataError>
    {
        let response = self.http.get(DEPTH_REST).send().await.map_err(|_| {
            MarketDataError::Disconnected("depth snapshot REST transport failed".into())
        })?;
        if response.status() != StatusCode::OK
            || response
                .content_length()
                .is_some_and(|n| n > 2 * 1024 * 1024)
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
        let url = format!("{PUBLIC_WS}/{stream}");
        let (mut socket, _) = timeout(Duration::from_secs(5), connect_async(&url))
            .await
            .map_err(|_| MarketDataError::Disconnected("public websocket connect timeout".into()))?
            .map_err(|_| {
                MarketDataError::Disconnected("public websocket handshake failed".into())
            })?;
        match &spec.kind {
            FeedKind::L2Book => self.stream_depth(&mut socket, &sink).await,
            kind => self.stream_simple(&mut socket, kind, &sink).await,
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
        assert!(
            public_stream(&spec(
                SYMBOL,
                Venue::BinancePm,
                FeedKind::Candle {
                    interval_ns: 60_000_000_000
                }
            ))
            .is_err()
        );
    }

    #[test]
    fn client_construction_does_not_start_network_or_trading() {
        assert!(BinanceMarketDataSource::new().is_ok());
    }
}
