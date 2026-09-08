//! Hyperliquid adapter boundary.
//!
//! Venue-specific signing, nonce, websocket recovery and SDK types stay here.

#[cfg(feature = "sdk")]
pub use hyperliquid_rust_sdk as sdk;

#[derive(Debug, Clone)]
pub struct HyperliquidConfig {
    pub api_url: String,
    pub ws_url: String,
    pub account_address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperliquidFeed {
    Trades,
    BestBidAsk,
    L2Book,
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
