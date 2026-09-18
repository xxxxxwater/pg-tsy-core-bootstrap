//! Binance USD-M depth snapshot/diff bridge for a single symbol.
//!
//! Pure normalization; transport must buffer WS deltas while REST snapshot loads.
//! A book is NEVER publishable until the first delta bridges the snapshot; every
//! following delta must have `pu == previous u`. A gap requires a fresh snapshot.

use std::collections::{BTreeMap, VecDeque};
use std::str::FromStr;

use pg_types::Venue;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

use crate::{BookLevel, L2Book};

const MAX_BUFFER: usize = 2048;
const PUBLISHED_LEVELS: usize = 50;

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceDepthSnapshot {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceDepthDelta {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "E")]
    pub event_time_ms: u64,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub final_update_id: u64,
    pub pu: u64,
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DepthError {
    #[error("wrong symbol or malformed depth event")]
    InvalidEvent,
    #[error("invalid price or quantity")]
    InvalidLevel,
    #[error("delta sequence gap; fetch a new snapshot")]
    SequenceGap,
    #[error("depth buffer limit exceeded; resubscribe and resnapshot")]
    BufferFull,
    #[error("crossed or empty book; fetch a new snapshot")]
    InvalidBook,
}

#[derive(Default)]
pub struct BinanceDepthBridge {
    symbol: String,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    last_update_id: Option<u64>,
    bridged: bool,
    buffered: VecDeque<(BinanceDepthDelta, u64)>,
}

impl BinanceDepthBridge {
    pub fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_owned(),
            ..Self::default()
        }
    }

    pub fn is_ready(&self) -> bool {
        self.bridged
    }

    pub fn invalidate(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_update_id = None;
        self.bridged = false;
        self.buffered.clear();
    }

    /// May return None while waiting for a REST snapshot or bridge event.
    pub fn push(
        &mut self,
        delta: BinanceDepthDelta,
        received_ns: u64,
    ) -> Result<Option<L2Book>, DepthError> {
        if delta.symbol != self.symbol
            || delta.first_update_id > delta.final_update_id
            || delta.event_time_ms == 0
            || received_ns == 0
        {
            self.invalidate();
            return Err(DepthError::InvalidEvent);
        }
        if self.last_update_id.is_none() {
            if self.buffered.len() >= MAX_BUFFER {
                self.invalidate();
                return Err(DepthError::BufferFull);
            }
            self.buffered.push_back((delta, received_ns));
            return Ok(None);
        }
        self.apply(delta, received_ns)
    }

    /// Install a fresh REST snapshot, then replay all previously buffered diffs.
    /// Do not trade on a snapshot alone: publication requires sequence bridging.
    pub fn install_snapshot(
        &mut self,
        snapshot: BinanceDepthSnapshot,
    ) -> Result<Option<L2Book>, DepthError> {
        let bids = match parse_levels(&snapshot.bids) {
            Ok(levels) => levels,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        let asks = match parse_levels(&snapshot.asks) {
            Ok(levels) => levels,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        self.bids = bids;
        self.asks = asks;
        self.last_update_id = Some(snapshot.last_update_id);
        self.bridged = false;
        if !self.valid_book() {
            self.invalidate();
            return Err(DepthError::InvalidBook);
        }
        let mut latest = None;
        while let Some((delta, received_ns)) = self.buffered.pop_front() {
            match self.apply(delta, received_ns) {
                Ok(Some(book)) => latest = Some(book),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(latest)
    }

    fn apply(
        &mut self,
        delta: BinanceDepthDelta,
        received_ns: u64,
    ) -> Result<Option<L2Book>, DepthError> {
        let last = self.last_update_id.ok_or(DepthError::SequenceGap)?;
        if delta.final_update_id <= last {
            return Ok(None);
        }
        let sequence_valid = if self.bridged {
            delta.pu == last
        } else {
            delta.first_update_id <= last && delta.final_update_id >= last
        };
        if !sequence_valid {
            self.invalidate();
            return Err(DepthError::SequenceGap);
        }
        // Validate the complete event before changing either book side.
        let bids = match parse_updates(&delta.bids) {
            Ok(levels) => levels,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        let asks = match parse_updates(&delta.asks) {
            Ok(levels) => levels,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        for (price, quantity) in bids {
            if quantity.is_zero() {
                self.bids.remove(&price);
            } else {
                self.bids.insert(price, quantity);
            }
        }
        for (price, quantity) in asks {
            if quantity.is_zero() {
                self.asks.remove(&price);
            } else {
                self.asks.insert(price, quantity);
            }
        }
        if !self.valid_book() {
            self.invalidate();
            return Err(DepthError::InvalidBook);
        }
        self.bridged = true;
        self.last_update_id = Some(delta.final_update_id);
        let ts_event_ns = delta.event_time_ms.checked_mul(1_000_000).ok_or_else(|| {
            self.invalidate();
            DepthError::InvalidEvent
        })?;
        Ok(Some(L2Book {
            venue: Venue::BinancePm,
            asset: self.symbol.clone(),
            ts_event_ns,
            ts_recv_ns: received_ns,
            bids: self.bids.iter().rev().take(PUBLISHED_LEVELS).map(|(price, quantity)| {
                BookLevel { price: *price, quantity: *quantity, order_count: None }
            }).collect(),
            asks: self.asks.iter().take(PUBLISHED_LEVELS).map(|(price, quantity)| {
                BookLevel { price: *price, quantity: *quantity, order_count: None }
            }).collect(),
            sequence: Some(delta.final_update_id),
            is_snapshot: false,
        }))
    }

    fn valid_book(&self) -> bool {
        matches!((self.bids.keys().next_back(), self.asks.keys().next()),
            (Some(bid), Some(ask)) if bid < ask)
    }
}

fn parse_updates(levels: &[[String; 2]]) -> Result<Vec<(Decimal, Decimal)>, DepthError> {
    levels.iter().map(|[price, quantity]| {
        let price = Decimal::from_str(price).map_err(|_| DepthError::InvalidLevel)?;
        let quantity = Decimal::from_str(quantity).map_err(|_| DepthError::InvalidLevel)?;
        if price <= Decimal::ZERO || quantity < Decimal::ZERO {
            return Err(DepthError::InvalidLevel);
        }
        Ok((price, quantity))
    }).collect()
}

fn parse_levels(levels: &[[String; 2]]) -> Result<BTreeMap<Decimal, Decimal>, DepthError> {
    let mut book = BTreeMap::new();
    for (price, quantity) in parse_updates(levels)? {
        if !quantity.is_zero() {
            book.insert(price, quantity);
        }
    }
    Ok(book)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> BinanceDepthSnapshot {
        BinanceDepthSnapshot { last_update_id: 100,
            bids: vec![["99".into(), "2".into()]],
            asks: vec![["101".into(), "3".into()]], }
    }
    fn delta(first: u64, final_id: u64, previous: u64) -> BinanceDepthDelta {
        BinanceDepthDelta { symbol: "BTCUSDC".into(), event_time_ms: 1_700_000_000_000,
            first_update_id: first, final_update_id: final_id, pu: previous,
            bids: vec![["100".into(), "1".into()]], asks: vec![], }
    }

    #[test]
    fn snapshot_does_not_publish_until_bridged() {
        let mut bridge = BinanceDepthBridge::new("BTCUSDC");
        assert!(bridge.install_snapshot(snapshot()).unwrap().is_none());
        assert!(!bridge.is_ready());
        let book = bridge.push(delta(99, 101, 98), 1_700_000_000_001_000_000).unwrap().unwrap();
        assert!(bridge.is_ready());
        assert_eq!(book.asset, "BTCUSDC");
        assert_eq!(book.sequence, Some(101));
        assert_eq!(book.bids[0].price, Decimal::from(100));
        assert_eq!(bridge.push(delta(102, 104, 101), 1_700_000_000_002_000_000)
            .unwrap().unwrap().sequence, Some(104));
    }

    #[test]
    fn buffered_diffs_replay_after_snapshot() {
        let mut bridge = BinanceDepthBridge::new("BTCUSDC");
        assert!(bridge.push(delta(99, 101, 98), 1).unwrap().is_none());
        assert_eq!(bridge.install_snapshot(snapshot()).unwrap().unwrap().sequence, Some(101));
    }

    #[test]
    fn missing_previous_id_blocks_exposure_and_requires_snapshot() {
        let mut bridge = BinanceDepthBridge::new("BTCUSDC");
        bridge.install_snapshot(snapshot()).unwrap();
        bridge.push(delta(99, 101, 98), 1).unwrap();
        assert_eq!(bridge.push(delta(103, 104, 99), 2).unwrap_err(), DepthError::SequenceGap);
        assert!(!bridge.is_ready());
    }

    #[test]
    fn invalid_level_and_crossed_book_fail_closed() {
        let mut bridge = BinanceDepthBridge::new("BTCUSDC");
        bridge.install_snapshot(snapshot()).unwrap();
        let mut bad = delta(99, 101, 98);
        bad.bids[0][1] = "-1".into();
        assert_eq!(bridge.push(bad, 1).unwrap_err(), DepthError::InvalidLevel);
        assert!(!bridge.is_ready());
        bridge.install_snapshot(snapshot()).unwrap();
        let mut crossed = delta(99, 101, 98);
        crossed.bids[0][0] = "102".into();
        assert_eq!(bridge.push(crossed, 1).unwrap_err(), DepthError::InvalidBook);
        assert!(!bridge.is_ready());
    }
}
