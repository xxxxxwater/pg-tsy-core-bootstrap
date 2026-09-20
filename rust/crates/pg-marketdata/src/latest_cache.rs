use crate::MarketEvent;
use pg_types::AssetKey;
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

#[derive(Debug, Default)]
pub struct LatestEventCache {
    inner: RwLock<BTreeMap<AssetKey, Arc<MarketEvent>>>,
}

impl LatestEventCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, event: MarketEvent) -> Arc<MarketEvent> {
        let key = event.asset_key();
        let value = Arc::new(event);
        self.inner
            .write()
            .expect("latest-event cache write lock poisoned")
            .insert(key, Arc::clone(&value));
        value
    }

    pub fn get(&self, key: &AssetKey) -> Option<Arc<MarketEvent>> {
        self.inner
            .read()
            .expect("latest-event cache read lock poisoned")
            .get(key)
            .cloned()
    }

    pub fn remove(&self, key: &AssetKey) -> Option<Arc<MarketEvent>> {
        self.inner
            .write()
            .expect("latest-event cache write lock poisoned")
            .remove(key)
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("latest-event cache read lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BestBidAsk;
    use pg_types::Venue;
    use rust_decimal::Decimal;

    #[test]
    fn latest_value_replaces_without_deep_copy_on_read() {
        let cache = LatestEventCache::new();
        let key = AssetKey::new(Venue::BinancePm, "BTCUSDC");

        let first = cache.insert(MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::BinancePm,
            asset: "BTCUSDC".into(),
            ts_event_ns: 1,
            ts_recv_ns: 2,
            bid_price: Decimal::from(99),
            bid_quantity: Decimal::ONE,
            ask_price: Decimal::from(100),
            ask_quantity: Decimal::ONE,
            sequence: Some(1),
        }));
        let read = cache.get(&key).unwrap();
        assert!(Arc::ptr_eq(&first, &read));

        cache.insert(MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::BinancePm,
            asset: "BTCUSDC".into(),
            ts_event_ns: 3,
            ts_recv_ns: 4,
            bid_price: Decimal::from(100),
            bid_quantity: Decimal::ONE,
            ask_price: Decimal::from(101),
            ask_quantity: Decimal::ONE,
            sequence: Some(2),
        }));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key).unwrap().ts_recv_ns(), 4);
    }
}
