use async_trait::async_trait;
use pg_types::Venue;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductType {
    Perpetual,
    Stock,
    Etf,
    Future,
    Forex,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentDescriptor {
    pub venue: Venue,
    pub symbol: String,
    pub product_type: ProductType,
    pub venue_instrument_id: Option<String>,
    pub base: Option<String>,
    pub quote: Option<String>,
    pub exchange: Option<String>,
    pub primary_exchange: Option<String>,
    pub currency: Option<String>,
    pub size_decimals: Option<u32>,
    pub tick_size: Option<Decimal>,
    pub lot_size: Option<Decimal>,
    pub min_size: Option<Decimal>,
    pub mark_price: Option<Decimal>,
    pub mid_price: Option<Decimal>,
    /// BBO-derived spread. Providers must leave this `None` rather than infer a
    /// fake spread from mark/mid or unrelated prices.
    pub spread_bps: Option<Decimal>,
    pub day_notional_volume: Option<Decimal>,
    pub open_interest: Option<Decimal>,
    pub funding_rate: Option<Decimal>,
    pub max_leverage: Option<u32>,
    pub tradable: bool,
}

impl InstrumentDescriptor {
    pub fn key(&self) -> String {
        format!("{:?}:{}", self.venue, self.symbol)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UniverseSnapshot {
    pub venue: Venue,
    pub discovered_at_ns: u64,
    pub instruments: Vec<InstrumentDescriptor>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UniverseFilter {
    pub min_day_notional_volume: Option<Decimal>,
    pub min_open_interest: Option<Decimal>,
    pub max_spread_bps: Option<Decimal>,
    pub min_price: Option<Decimal>,
    pub max_price: Option<Decimal>,
    #[serde(default)]
    pub include_symbols: BTreeSet<String>,
    #[serde(default)]
    pub exclude_symbols: BTreeSet<String>,
    pub top_n: Option<usize>,
}

impl UniverseFilter {
    pub fn apply(&self, snapshot: &UniverseSnapshot) -> Vec<InstrumentDescriptor> {
        let mut selected = snapshot
            .instruments
            .iter()
            .filter(|instrument| instrument.tradable)
            .filter(|instrument| {
                self.include_symbols.is_empty() || self.include_symbols.contains(&instrument.symbol)
            })
            .filter(|instrument| !self.exclude_symbols.contains(&instrument.symbol))
            .filter(|instrument| match self.min_day_notional_volume {
                Some(minimum) => instrument
                    .day_notional_volume
                    .is_some_and(|value| value >= minimum),
                None => true,
            })
            .filter(|instrument| match self.min_open_interest {
                Some(minimum) => instrument
                    .open_interest
                    .is_some_and(|value| value >= minimum),
                None => true,
            })
            // Spread is safety-sensitive: when a max spread was requested, an
            // instrument with no trustworthy BBO spread is excluded rather than
            // treated as liquid.
            .filter(|instrument| match self.max_spread_bps {
                Some(maximum) => instrument.spread_bps.is_some_and(|value| value <= maximum),
                None => true,
            })
            .filter(|instrument| match self.min_price {
                Some(minimum) => instrument
                    .mark_price
                    .or(instrument.mid_price)
                    .is_some_and(|value| value >= minimum),
                None => true,
            })
            .filter(|instrument| match self.max_price {
                Some(maximum) => instrument
                    .mark_price
                    .or(instrument.mid_price)
                    .is_some_and(|value| value <= maximum),
                None => true,
            })
            .cloned()
            .collect::<Vec<_>>();

        // Crypto metadata supplies comparable venue-wide notional volume, so rank
        // by it when available. Scanner-style providers (for example IBKR) already
        // return a meaningful venue rank but may not supply a comparable 24h
        // notional. Preserve provider order in that case rather than silently
        // replacing the venue rank with alphabetical symbol order.
        if selected
            .iter()
            .any(|instrument| instrument.day_notional_volume.is_some())
        {
            selected.sort_by(|left, right| {
                right
                    .day_notional_volume
                    .unwrap_or(Decimal::ZERO)
                    .cmp(&left.day_notional_volume.unwrap_or(Decimal::ZERO))
                    .then_with(|| left.symbol.cmp(&right.symbol))
            });
        }
        if let Some(top_n) = self.top_n {
            selected.truncate(top_n);
        }
        selected
    }
}

#[derive(Debug, Error)]
pub enum UniverseError {
    #[error("universe transport error: {0}")]
    Transport(String),
    #[error("universe authentication failed: {0}")]
    Authentication(String),
    #[error("universe conversion failed: {0}")]
    Conversion(String),
    #[error("universe discovery is unsupported: {0}")]
    Unsupported(String),
}

#[async_trait]
pub trait UniverseProvider: Send + Sync {
    fn venue(&self) -> Venue;

    async fn discover(&self) -> Result<UniverseSnapshot, UniverseError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(symbol: &str, volume: i64, spread: Option<i64>) -> InstrumentDescriptor {
        InstrumentDescriptor {
            venue: Venue::Hyperliquid,
            symbol: symbol.into(),
            product_type: ProductType::Perpetual,
            venue_instrument_id: None,
            base: Some(symbol.into()),
            quote: Some("USDC".into()),
            exchange: Some("HYPERLIQUID".into()),
            primary_exchange: None,
            currency: Some("USDC".into()),
            size_decimals: Some(2),
            tick_size: None,
            lot_size: None,
            min_size: None,
            mark_price: Some(Decimal::from(10)),
            mid_price: Some(Decimal::from(10)),
            spread_bps: spread.map(Decimal::from),
            day_notional_volume: Some(Decimal::from(volume)),
            open_interest: Some(Decimal::from(1_000_000)),
            funding_rate: Some(Decimal::ZERO),
            max_leverage: Some(20),
            tradable: true,
        }
    }

    fn scanner_descriptor(symbol: &str) -> InstrumentDescriptor {
        InstrumentDescriptor {
            venue: Venue::InteractiveBrokers,
            symbol: symbol.into(),
            product_type: ProductType::Stock,
            venue_instrument_id: Some(format!("conid-{symbol}")),
            base: Some(symbol.into()),
            quote: Some("USD".into()),
            exchange: Some("SMART".into()),
            primary_exchange: None,
            currency: Some("USD".into()),
            size_decimals: None,
            tick_size: Some(Decimal::new(1, 2)),
            lot_size: None,
            min_size: None,
            mark_price: None,
            mid_price: None,
            spread_bps: None,
            day_notional_volume: None,
            open_interest: None,
            funding_rate: None,
            max_leverage: None,
            tradable: true,
        }
    }

    #[test]
    fn filters_and_ranks_by_real_volume() {
        let snapshot = UniverseSnapshot {
            venue: Venue::Hyperliquid,
            discovered_at_ns: 1,
            instruments: vec![
                descriptor("A", 5_000_000, Some(5)),
                descriptor("B", 10_000_000, Some(8)),
                descriptor("C", 20_000_000, Some(30)),
            ],
        };
        let filter = UniverseFilter {
            min_day_notional_volume: Some(Decimal::from(4_000_000)),
            max_spread_bps: Some(Decimal::from(20)),
            top_n: Some(1),
            ..UniverseFilter::default()
        };
        let selected = filter.apply(&snapshot);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].symbol, "B");
    }

    #[test]
    fn preserves_scanner_rank_when_volume_is_unavailable() {
        let snapshot = UniverseSnapshot {
            venue: Venue::InteractiveBrokers,
            discovered_at_ns: 1,
            instruments: vec![
                scanner_descriptor("ZZZ"),
                scanner_descriptor("AAA"),
                scanner_descriptor("MMM"),
            ],
        };
        let filter = UniverseFilter {
            top_n: Some(2),
            ..UniverseFilter::default()
        };
        let selected = filter.apply(&snapshot);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].symbol, "ZZZ");
        assert_eq!(selected[1].symbol, "AAA");
    }

    #[test]
    fn unknown_spread_fails_closed_when_spread_filter_is_enabled() {
        let snapshot = UniverseSnapshot {
            venue: Venue::Hyperliquid,
            discovered_at_ns: 1,
            instruments: vec![descriptor("A", 5_000_000, None)],
        };
        let filter = UniverseFilter {
            max_spread_bps: Some(Decimal::from(20)),
            ..UniverseFilter::default()
        };
        assert!(filter.apply(&snapshot).is_empty());
    }
}
