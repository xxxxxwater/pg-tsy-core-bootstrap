use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::{FeedSpec, MarketDataError, MarketDataSource, MarketEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    Connected,
    Degraded,
    Reconnecting,
    Stale,
    Failed,
}

#[derive(Debug, Clone)]
pub struct SubscriptionStatus {
    pub state: SubscriptionState,
    pub reconnect_count: u64,
    pub last_event_ns: Option<u64>,
}

#[derive(Debug)]
pub struct SubscriptionSupervisor<S> {
    source: S,
    feeds: BTreeMap<String, FeedSpec>,
    statuses: BTreeMap<String, SubscriptionStatus>,
}

impl<S: MarketDataSource> SubscriptionSupervisor<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            feeds: BTreeMap::new(),
            statuses: BTreeMap::new(),
        }
    }

    pub fn apply_derived_feeds<I>(&mut self, specs: I)
    where
        I: IntoIterator<Item = FeedSpec>,
    {
        self.feeds.clear();
        for spec in specs {
            let key = format!("{:?}:{}:{:?}", spec.venue, spec.asset, spec.kind);
            self.statuses.entry(key.clone()).or_insert(SubscriptionStatus {
                state: SubscriptionState::Reconnecting,
                reconnect_count: 0,
                last_event_ns: None,
            });
            self.feeds.insert(key, spec);
        }
    }

    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    pub async fn start_once(
        &mut self,
        sink: mpsc::Sender<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        let feeds = self.feeds.values().cloned().collect::<Vec<_>>();
        for spec in feeds {
            self.source.stream(spec, sink.clone()).await?;
        }
        Ok(())
    }

    pub fn observe(&mut self, event: &MarketEvent) {
        for status in self.statuses.values_mut() {
            status.last_event_ns = Some(event.ts_recv_ns());
            status.state = SubscriptionState::Connected;
        }
    }
}
