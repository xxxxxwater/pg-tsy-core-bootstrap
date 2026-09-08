//! Interactive Brokers TWS / IB Gateway adapter boundary.
//!
//! `ibapi` is a community Rust implementation, not an official IBKR Rust SDK.
//! Keep its types inside this crate so it can be replaced without touching core.

#[cfg(feature = "sdk")]
pub use ibapi as sdk;

#[derive(Debug, Clone)]
pub struct IbkrConfig {
    pub gateway_addr: String,
    pub client_id: i32,
    pub account: Option<String>,
    pub market_depth_rows: i32,
}

#[derive(Debug, Clone)]
pub struct IbkrStockSpec {
    pub symbol: String,
    pub exchange: String,
    pub currency: String,
    pub primary_exchange: Option<String>,
    pub con_id: Option<i32>,
}

impl IbkrStockSpec {
    pub fn smart_us(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            exchange: "SMART".into(),
            currency: "USD".into(),
            primary_exchange: None,
            con_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IbkrFeed {
    TickByTickTrades,
    TickByTickBidAsk,
    MarketDepth,
    RealtimeBars,
    OrderEvents,
    AccountUpdates,
}

pub struct IbkrAdapter {
    pub config: IbkrConfig,
}

impl IbkrAdapter {
    pub fn new(config: IbkrConfig) -> Self {
        Self { config }
    }

    pub fn feeds_for_live_trading() -> &'static [IbkrFeed] {
        &[
            IbkrFeed::TickByTickTrades,
            IbkrFeed::TickByTickBidAsk,
            IbkrFeed::MarketDepth,
            IbkrFeed::OrderEvents,
            IbkrFeed::AccountUpdates,
        ]
    }
}

#[cfg(feature = "sdk")]
mod live_market_data {
    use super::{sdk, IbkrConfig, IbkrStockSpec};
    use async_trait::async_trait;
    use futures::StreamExt;
    use pg_marketdata::{
        AggressorSide, BestBidAsk, BookLevel, Candle, FeedKind, FeedSpec, L2Book, MarketDataError,
        MarketDataSource, MarketEvent, TradeTick,
    };
    use pg_types::Venue;
    use rust_decimal::Decimal;
    use std::{
        str::FromStr,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::mpsc;

    use sdk::market_data::{realtime::MarketDepths, SmartDepth};
    use sdk::prelude::*;
    use sdk::subscriptions::SubscriptionItemStreamExt;

    pub struct IbkrMarketDataSource {
        client: Arc<sdk::Client>,
        instrument: IbkrStockSpec,
        depth_rows: i32,
    }

    impl IbkrMarketDataSource {
        pub async fn connect(
            config: &IbkrConfig,
            instrument: IbkrStockSpec,
        ) -> Result<Self, MarketDataError> {
            if config.market_depth_rows <= 0 {
                return Err(MarketDataError::Subscription(
                    "IBKR market_depth_rows must be positive".into(),
                ));
            }
            let client = sdk::Client::connect(&config.gateway_addr, config.client_id)
                .await
                .map_err(|error| MarketDataError::Disconnected(error.to_string()))?;
            Ok(Self {
                client: Arc::new(client),
                instrument,
                depth_rows: config.market_depth_rows,
            })
        }

        fn contract(&self) -> sdk::contracts::Contract {
            let mut builder = sdk::contracts::Contract::stock(&self.instrument.symbol)
                .on_exchange(&self.instrument.exchange)
                .in_currency(&self.instrument.currency);
            if let Some(primary) = &self.instrument.primary_exchange {
                builder = builder.primary(primary);
            }
            let mut contract = builder.build();
            if let Some(con_id) = self.instrument.con_id {
                contract.contract_id = con_id;
            }
            contract
        }
    }

    #[async_trait]
    impl MarketDataSource for IbkrMarketDataSource {
        async fn stream(
            &mut self,
            spec: FeedSpec,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            if spec.venue != Venue::InteractiveBrokers {
                return Err(MarketDataError::Subscription(
                    "IBKR source received non-IBKR feed".into(),
                ));
            }
            if spec.asset != self.instrument.symbol {
                return Err(MarketDataError::Subscription(format!(
                    "feed asset {} does not match configured IBKR instrument {}",
                    spec.asset, self.instrument.symbol
                )));
            }

            match spec.kind {
                FeedKind::Trades => self.stream_trades(&spec.asset, sink).await,
                FeedKind::BestBidAsk => self.stream_bbo(&spec.asset, sink).await,
                FeedKind::L2Book => self.stream_depth(&spec.asset, sink).await,
                FeedKind::Candle { interval_ns } => {
                    self.stream_realtime_bars(&spec.asset, interval_ns, sink)
                        .await
                }
            }
        }
    }

    impl IbkrMarketDataSource {
        async fn stream_trades(
            &self,
            asset: &str,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            let contract = self.contract();
            let subscription = self
                .client
                .tick_by_tick(&contract, 0)
                .all_last()
                .await
                .map_err(subscription_error)?;
            let mut stream = subscription.filter_data();
            while let Some(item) = stream.next().await {
                let trade = item.map_err(disconnected_error)?;
                let event = MarketEvent::Trade(TradeTick {
                    venue: Venue::InteractiveBrokers,
                    asset: asset.into(),
                    ts_event_ns: offset_ns(trade.time)?,
                    ts_recv_ns: now_ns()?,
                    price: decimal(trade.price)?,
                    quantity: decimal(trade.size)?,
                    aggressor: AggressorSide::Unknown,
                    sequence: None,
                });
                send(&sink, event).await?;
            }
            Err(MarketDataError::Disconnected(
                "IBKR tick-by-tick trade stream ended".into(),
            ))
        }

        async fn stream_bbo(
            &self,
            asset: &str,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            let contract = self.contract();
            let subscription = self
                .client
                .tick_by_tick(&contract, 0)
                .bid_ask(IgnoreSize::No)
                .await
                .map_err(subscription_error)?;
            let mut stream = subscription.filter_data();
            while let Some(item) = stream.next().await {
                let quote = item.map_err(disconnected_error)?;
                let event = MarketEvent::BestBidAsk(BestBidAsk {
                    venue: Venue::InteractiveBrokers,
                    asset: asset.into(),
                    ts_event_ns: offset_ns(quote.time)?,
                    ts_recv_ns: now_ns()?,
                    bid_price: decimal(quote.bid_price)?,
                    bid_quantity: decimal(quote.bid_size)?,
                    ask_price: decimal(quote.ask_price)?,
                    ask_quantity: decimal(quote.ask_size)?,
                    sequence: None,
                });
                send(&sink, event).await?;
            }
            Err(MarketDataError::Disconnected(
                "IBKR tick-by-tick bid/ask stream ended".into(),
            ))
        }

        async fn stream_depth(
            &self,
            asset: &str,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            let contract = self.contract();
            let subscription = self
                .client
                .market_depth(&contract, self.depth_rows)
                .smart_depth(SmartDepth::No)
                .subscribe()
                .await
                .map_err(subscription_error)?;
            let mut stream = subscription.filter_data();
            let mut book = PositionBook::new(self.depth_rows as usize);

            while let Some(item) = stream.next().await {
                let update = item.map_err(disconnected_error)?;
                match update {
                    MarketDepths::MarketDepth(depth) => {
                        book.apply(
                            depth.side,
                            depth.operation,
                            depth.position,
                            depth.price,
                            depth.size,
                        )?;
                    }
                    MarketDepths::MarketDepthL2(depth) => {
                        book.apply(
                            depth.side,
                            depth.operation,
                            depth.position,
                            depth.price,
                            depth.size,
                        )?;
                    }
                }
                let recv_ns = now_ns()?;
                send(&sink, MarketEvent::L2Book(book.event(asset, recv_ns)?)).await?;
            }

            Err(MarketDataError::Disconnected(
                "IBKR market depth stream ended".into(),
            ))
        }

        async fn stream_realtime_bars(
            &self,
            asset: &str,
            interval_ns: u64,
            sink: mpsc::Sender<MarketEvent>,
        ) -> Result<(), MarketDataError> {
            const FIVE_SECONDS_NS: u64 = 5_000_000_000;
            if interval_ns != FIVE_SECONDS_NS {
                return Err(MarketDataError::Subscription(format!(
                    "IBKR realtime bars support 5s only; requested {interval_ns}ns"
                )));
            }
            let contract = self.contract();
            let subscription = self
                .client
                .realtime_bars(&contract)
                .subscribe()
                .await
                .map_err(subscription_error)?;
            let mut stream = subscription.filter_data();
            while let Some(item) = stream.next().await {
                let bar = item.map_err(disconnected_error)?;
                let start_ns = offset_ns(bar.date)?;
                send(
                    &sink,
                    MarketEvent::Candle(Candle {
                        venue: Venue::InteractiveBrokers,
                        asset: asset.into(),
                        interval_ns,
                        start_ns,
                        end_ns: start_ns.saturating_add(interval_ns),
                        ts_recv_ns: now_ns()?,
                        open: decimal(bar.open)?,
                        high: decimal(bar.high)?,
                        low: decimal(bar.low)?,
                        close: decimal(bar.close)?,
                        volume: decimal(bar.volume)?,
                        trades: u64::try_from(bar.count).unwrap_or_default(),
                    }),
                )
                .await?;
            }
            Err(MarketDataError::Disconnected(
                "IBKR realtime bar stream ended".into(),
            ))
        }
    }

    #[derive(Debug, Clone)]
    struct PositionBook {
        depth: usize,
        bids: Vec<Option<(f64, f64)>>,
        asks: Vec<Option<(f64, f64)>>,
    }

    impl PositionBook {
        fn new(depth: usize) -> Self {
            Self {
                depth,
                bids: vec![None; depth],
                asks: vec![None; depth],
            }
        }

        fn apply(
            &mut self,
            side: i32,
            operation: i32,
            position: i32,
            price: f64,
            size: f64,
        ) -> Result<(), MarketDataError> {
            let position = usize::try_from(position).map_err(|_| {
                MarketDataError::Conversion("negative IBKR market depth position".into())
            })?;
            let depth = self.depth;
            let levels = if side == 1 {
                &mut self.bids
            } else {
                &mut self.asks
            };
            if position >= levels.len() {
                return Ok(());
            }

            match operation {
                0 => {
                    levels.insert(position, Some((price, size)));
                    levels.truncate(depth);
                }
                1 => levels[position] = Some((price, size)),
                2 => {
                    levels.remove(position);
                    levels.push(None);
                }
                other => {
                    return Err(MarketDataError::Conversion(format!(
                        "unknown IBKR market depth operation {other}"
                    )));
                }
            }
            Ok(())
        }

        fn event(&self, asset: &str, recv_ns: u64) -> Result<L2Book, MarketDataError> {
            Ok(L2Book {
                venue: Venue::InteractiveBrokers,
                asset: asset.into(),
                ts_event_ns: recv_ns,
                ts_recv_ns: recv_ns,
                bids: levels(&self.bids)?,
                asks: levels(&self.asks)?,
                sequence: None,
                is_snapshot: false,
            })
        }
    }

    fn levels(input: &[Option<(f64, f64)>]) -> Result<Vec<BookLevel>, MarketDataError> {
        input
            .iter()
            .flatten()
            .map(|(price, quantity)| {
                Ok(BookLevel {
                    price: decimal(*price)?,
                    quantity: decimal(*quantity)?,
                    order_count: None,
                })
            })
            .collect()
    }

    async fn send(
        sink: &mpsc::Sender<MarketEvent>,
        event: MarketEvent,
    ) -> Result<(), MarketDataError> {
        sink.send(event)
            .await
            .map_err(|_| MarketDataError::Disconnected("market event sink closed".into()))
    }

    fn decimal(value: f64) -> Result<Decimal, MarketDataError> {
        if !value.is_finite() {
            return Err(MarketDataError::Conversion(
                "IBKR supplied non-finite numeric value".into(),
            ));
        }
        Decimal::from_str(&value.to_string())
            .map_err(|error| MarketDataError::Conversion(error.to_string()))
    }

    fn offset_ns(value: time::OffsetDateTime) -> Result<u64, MarketDataError> {
        u64::try_from(value.unix_timestamp_nanos())
            .map_err(|_| MarketDataError::Conversion("IBKR timestamp is before Unix epoch".into()))
    }

    fn now_ns() -> Result<u64, MarketDataError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .map_err(|error| MarketDataError::Conversion(error.to_string()))
    }

    fn subscription_error(error: sdk::Error) -> MarketDataError {
        MarketDataError::Subscription(error.to_string())
    }

    fn disconnected_error(error: sdk::Error) -> MarketDataError {
        MarketDataError::Disconnected(error.to_string())
    }
}

#[cfg(feature = "sdk")]
pub use live_market_data::IbkrMarketDataSource;
