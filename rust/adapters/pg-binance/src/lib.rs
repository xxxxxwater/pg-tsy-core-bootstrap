//! Binance adapter boundary.
//!
//! Production implementation should wrap Binance's official Rust SDK and map
//! Portfolio Margin / Portfolio Margin Pro semantics into pg-types. Keep all
//! venue-specific request/response types inside this crate.

pub struct BinancePmAdapter;
