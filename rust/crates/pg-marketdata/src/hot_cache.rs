use crate::MarketEvent;
use pg_types::AssetKey;
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};
use tokio::sync::watch;

#[derive(Debug, Clone, Default)]
pub struct HotMarketCache {
    inner: Arc<RwLock<BTreeMap<AssetKey, watch::Sender<Option<MarketEvent>>>>>,
}

impl HotMarketCache {
    pub fn publish(&self, key: AssetKey, event: MarketEvent) {
        let sender = {
            let mut guard = self.inner.write().expect("hot market cache poisoned");
            guard
                .entry(key)
                .or_insert_with(|| watch::channel(None).0)
                .clone()
        };
        sender.send_replace(Some(event));
    }

    pub fn latest(&self, key: &AssetKey) -> Option<MarketEvent> {
        let sender = self
            .inner
            .read()
            .expect("hot market cache poisoned")
            .get(key)?
            .clone();
        sender.borrow().clone()
    }

    pub fn subscribe(&self, key: AssetKey) -> watch::Receiver<Option<MarketEvent>> {
        let sender = {
            let mut guard = self.inner.write().expect("hot market cache poisoned");
            guard
                .entry(key)
                .or_insert_with(|| watch::channel(None).0)
                .clone()
        };
        sender.subscribe()
    }

    pub fn tracked_assets(&self) -> usize {
        self.inner.read().expect("hot market cache poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BestBidAsk, MarketEvent};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    fn bbo(asset: &str, recv: u64) -> MarketEvent {
        MarketEvent::BestBidAsk(BestBidAsk {
            venue: Venue::BinancePm,
            asset: asset.into(),
            ts_event_ns: recv - 1,
            ts_recv_ns: recv,
            bid_price: Decimal::from(99),
            bid_quantity: Decimal::ONE,
            ask_price: Decimal::from(100),
            ask_quantity: Decimal::ONE,
            sequence: Some(recv),
        })
    }

    #[tokio::test]
    async fn latest_quote_is_replaced_without_queue_growth() {
        let cache = HotMarketCache::default();
        let key = AssetKey::new(Venue::BinancePm, "BTCUSDC");
        let mut rx = cache.subscribe(key.clone());
        cache.publish(key.clone(), bbo("BTCUSDC", 10));
        cache.publish(key.clone(), bbo("BTCUSDC", 11));

        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().as_ref().unwrap().ts_recv_ns(), 11);
        assert_eq!(cache.latest(&key).unwrap().ts_recv_ns(), 11);
        assert_eq!(cache.tracked_assets(), 1);
    }
}
