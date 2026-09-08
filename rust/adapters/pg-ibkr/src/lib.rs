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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IbkrFeed {
    TickByTickTrades,
    TickByTickBidAsk,
    MarketData,
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
            IbkrFeed::OrderEvents,
            IbkrFeed::AccountUpdates,
        ]
    }
}
