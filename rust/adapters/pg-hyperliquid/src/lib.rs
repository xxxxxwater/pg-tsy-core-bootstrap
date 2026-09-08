//! Hyperliquid adapter boundary.
//!
//! Venue-specific signing, nonce, websocket recovery and SDK types stay here.

#[cfg(feature = "sdk")]
pub use hyperliquid_rust_sdk as sdk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperliquidNetwork {
    Mainnet,
    Testnet,
}

#[derive(Debug, Clone)]
pub struct HyperliquidConfig {
    pub network: HyperliquidNetwork,
    pub account_address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperliquidFeed {
    Trades,
    BestBidAsk,
    L2Book,
    Candle,
    UserEvents,
}

pub struct HyperliquidAdapter {
    pub config: HyperliquidConfig,
}

impl HyperliquidAdapter {
    pub fn new(config: HyperliquidConfig) -> Self {
        Self { config }
    }

    pub fn feeds_for_live_trading() -> &'static [HyperliquidFeed] {
        &[
            HyperliquidFeed::Trades,
            HyperliquidFeed::BestBidAsk,
            HyperliquidFeed::L2Book,
            HyperliquidFeed::UserEvents,
        ]
    }
}

#[cfg(feature = "sdk")]
mod live_market_data {
    use super::{sdk, HyperliquidNetwork};
    use async_trait::async_trait;
    use pg_marketdata::{
        AggressorSide, BestBidAsk, BookLevel, Candle, FeedKind, FeedSpec, L2Book, MarketDataError,
        MarketDataSource, MarketEvent, TradeTick,
    };
    use pg_types::Venue;
    use rust_decimal::Decimal;
    use std::{
        str::FromStr,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::mpsc;

    pub struct HyperliquidMarketDataSource {
        client: sdk::InfoClient,
    }

    impl HyperliquidMarketDataSource {
        pub async fn connect(network: HyperliquidNetwork) -> Result<Self, MarketDataError> {
            let base_url = match network {
                HyperliquidNetwork::Mainnet => sdk::BaseUrl::Mainnet,
                HyperliquidNetwork::Testnet => sdk::BaseUrl::Testnet,
            };
            let client = sdk::InfoClient::with_reconnect(None, Some(base_url))
                .await
                .map_err(|error| MarketDataError::Disconnected(error.to_string()))?;
            Ok(Self { client })
        }
    }

    #[async_trait]
    impl MarketDataSource for HyperliquidMarketDataSource {
        async fn stream(
            &mut self,
            spec: FeedSpec,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            if spec.venue != Venue::Hyperliquid {
                return Err(MarketDataError::Subscription(
                    "Hyperliquid source received non-Hyperliquid feed".into(),
                ));
            }

            let subscription = subscription_for(&spec)?;
            let (sdk_tx, mut sdk_rx) = tokio::sync::mpsc::unbounded_channel();
            self.client
                .subscribe(subscription, sdk_tx)
                .await
                .map_err(|error| MarketDataError::Subscription(error.to_string()))?;

            while let Some(message) = sdk_rx.recv().await {
                let recv_ns = now_ns()?;
                for event in map_message(message, recv_ns)? {
                    sink.send(event).await.map_err(|_| {
                        MarketDataError::Disconnected("market event sink closed".into())
                    })?;
                }
            }

            Err(MarketDataError::Disconnected(
                "Hyperliquid websocket subscription ended".into(),
            ))
        }
    }

    fn subscription_for(spec: &FeedSpec) -> Result<sdk::Subscription, MarketDataError> {
        let coin = spec.asset.clone();
        match spec.kind {
            FeedKind::Trades => Ok(sdk::Subscription::Trades { coin }),
            FeedKind::BestBidAsk => Ok(sdk::Subscription::Bbo { coin }),
            FeedKind::L2Book => Ok(sdk::Subscription::L2Book { coin }),
            FeedKind::Candle { interval_ns } => Ok(sdk::Subscription::Candle {
                coin,
                interval: hl_interval(interval_ns)?.into(),
            }),
        }
    }

    fn map_message(
        message: sdk::Message,
        recv_ns: u64,
    ) -> Result<Vec<MarketEvent>, MarketDataError> {
        match message {
            sdk::Message::Trades(trades) => trades
                .data
                .into_iter()
                .map(|trade| {
                    Ok(MarketEvent::Trade(TradeTick {
                        venue: Venue::Hyperliquid,
                        asset: trade.coin,
                        ts_event_ns: millis_to_ns(trade.time),
                        ts_recv_ns: recv_ns,
                        price: decimal(&trade.px)?,
                        quantity: decimal(&trade.sz)?,
                        aggressor: aggressor(&trade.side),
                        sequence: Some(trade.tid),
                    }))
                })
                .collect(),
            sdk::Message::Bbo(bbo) => {
                let bid = bbo.data.bbo.first().and_then(Option::as_ref);
                let ask = bbo.data.bbo.get(1).and_then(Option::as_ref);
                match (bid, ask) {
                    (Some(bid), Some(ask)) => Ok(vec![MarketEvent::BestBidAsk(BestBidAsk {
                        venue: Venue::Hyperliquid,
                        asset: bbo.data.coin,
                        ts_event_ns: millis_to_ns(bbo.data.time),
                        ts_recv_ns: recv_ns,
                        bid_price: decimal(&bid.px)?,
                        bid_quantity: decimal(&bid.sz)?,
                        ask_price: decimal(&ask.px)?,
                        ask_quantity: decimal(&ask.sz)?,
                        sequence: None,
                    })]),
                    _ => Ok(Vec::new()),
                }
            }
            sdk::Message::L2Book(book) => {
                let bids = book
                    .data
                    .levels
                    .first()
                    .map(|levels| map_levels(levels))
                    .transpose()?
                    .unwrap_or_default();
                let asks = book
                    .data
                    .levels
                    .get(1)
                    .map(|levels| map_levels(levels))
                    .transpose()?
                    .unwrap_or_default();
                Ok(vec![MarketEvent::L2Book(L2Book {
                    venue: Venue::Hyperliquid,
                    asset: book.data.coin,
                    ts_event_ns: millis_to_ns(book.data.time),
                    ts_recv_ns: recv_ns,
                    bids,
                    asks,
                    sequence: None,
                    is_snapshot: true,
                })])
            }
            sdk::Message::Candle(candle) => Ok(vec![MarketEvent::Candle(Candle {
                venue: Venue::Hyperliquid,
                asset: candle.data.coin,
                interval_ns: parse_hl_interval(&candle.data.interval)?,
                start_ns: millis_to_ns(candle.data.time_open),
                end_ns: millis_to_ns(candle.data.time_close),
                ts_recv_ns: recv_ns,
                open: decimal(&candle.data.open)?,
                high: decimal(&candle.data.high)?,
                low: decimal(&candle.data.low)?,
                close: decimal(&candle.data.close)?,
                volume: decimal(&candle.data.volume)?,
                trades: candle.data.num_trades,
            })]),
            sdk::Message::HyperliquidError(error) => Err(MarketDataError::Disconnected(error)),
            _ => Ok(Vec::new()),
        }
    }

    fn map_levels(levels: &[sdk::BookLevel]) -> Result<Vec<BookLevel>, MarketDataError> {
        levels
            .iter()
            .map(|level| {
                Ok(BookLevel {
                    price: decimal(&level.px)?,
                    quantity: decimal(&level.sz)?,
                    order_count: Some(level.n),
                })
            })
            .collect()
    }

    fn aggressor(side: &str) -> AggressorSide {
        match side.to_ascii_uppercase().as_str() {
            "B" | "BUY" => AggressorSide::Buy,
            "A" | "S" | "SELL" => AggressorSide::Sell,
            _ => AggressorSide::Unknown,
        }
    }

    fn decimal(value: &str) -> Result<Decimal, MarketDataError> {
        Decimal::from_str(value).map_err(|error| MarketDataError::Conversion(error.to_string()))
    }

    fn millis_to_ns(value: u64) -> u64 {
        value.saturating_mul(1_000_000)
    }

    fn now_ns() -> Result<u64, MarketDataError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .map_err(|error| MarketDataError::Conversion(error.to_string()))
    }

    fn hl_interval(interval_ns: u64) -> Result<&'static str, MarketDataError> {
        match interval_ns {
            60_000_000_000 => Ok("1m"),
            180_000_000_000 => Ok("3m"),
            300_000_000_000 => Ok("5m"),
            900_000_000_000 => Ok("15m"),
            1_800_000_000_000 => Ok("30m"),
            3_600_000_000_000 => Ok("1h"),
            7_200_000_000_000 => Ok("2h"),
            14_400_000_000_000 => Ok("4h"),
            28_800_000_000_000 => Ok("8h"),
            43_200_000_000_000 => Ok("12h"),
            86_400_000_000_000 => Ok("1d"),
            other => Err(MarketDataError::Subscription(format!(
                "unsupported Hyperliquid candle interval ns={other}"
            ))),
        }
    }

    fn parse_hl_interval(interval: &str) -> Result<u64, MarketDataError> {
        match interval {
            "1m" => Ok(60_000_000_000),
            "3m" => Ok(180_000_000_000),
            "5m" => Ok(300_000_000_000),
            "15m" => Ok(900_000_000_000),
            "30m" => Ok(1_800_000_000_000),
            "1h" => Ok(3_600_000_000_000),
            "2h" => Ok(7_200_000_000_000),
            "4h" => Ok(14_400_000_000_000),
            "8h" => Ok(28_800_000_000_000),
            "12h" => Ok(43_200_000_000_000),
            "1d" => Ok(86_400_000_000_000),
            other => Err(MarketDataError::Conversion(format!(
                "unsupported Hyperliquid candle interval {other}"
            ))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_supported_intervals_both_ways() {
            for interval in [
                60_000_000_000,
                300_000_000_000,
                3_600_000_000_000,
                86_400_000_000_000,
            ] {
                let text = hl_interval(interval).unwrap();
                assert_eq!(parse_hl_interval(text).unwrap(), interval);
            }
        }
    }
}

#[cfg(feature = "sdk")]
pub use live_market_data::HyperliquidMarketDataSource;
