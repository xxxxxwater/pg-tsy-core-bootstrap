use std::collections::BTreeMap;

use crate::{FeedKind, FeedSpec, MarketEvent};

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

#[derive(Debug, Default)]
pub struct SubscriptionSupervisor {
    feeds: BTreeMap<String, FeedSpec>,
    statuses: BTreeMap<String, SubscriptionStatus>,
}

impl SubscriptionSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_derived_feeds<I>(&mut self, specs: I)
    where
        I: IntoIterator<Item = FeedSpec>,
    {
        let mut next_feeds = BTreeMap::new();
        for spec in specs {
            let key = feed_key(&spec);
            self.statuses
                .entry(key.clone())
                .or_insert(SubscriptionStatus {
                    state: SubscriptionState::Reconnecting,
                    reconnect_count: 0,
                    last_event_ns: None,
                });
            next_feeds.insert(key, spec);
        }
        self.statuses.retain(|key, _| next_feeds.contains_key(key));
        self.feeds = next_feeds;
    }

    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    pub fn connected_count(&self) -> usize {
        self.statuses
            .values()
            .filter(|status| status.state == SubscriptionState::Connected)
            .count()
    }

    pub fn all_connected(&self) -> bool {
        !self.feeds.is_empty() && self.connected_count() == self.feeds.len()
    }

    pub fn feeds(&self) -> impl Iterator<Item = &FeedSpec> {
        self.feeds.values()
    }

    pub fn statuses(&self) -> impl Iterator<Item = (&FeedSpec, &SubscriptionStatus)> {
        self.feeds.iter().filter_map(|(key, spec)| {
            self.statuses.get(key).map(|status| (spec, status))
        })
    }

    pub fn status(&self, spec: &FeedSpec) -> Option<&SubscriptionStatus> {
        self.statuses.get(&feed_key(spec))
    }

    pub fn observe(&mut self, event: &MarketEvent) -> bool {
        let spec = feed_spec_for_event(event);
        let key = feed_key(&spec);
        let Some(status) = self.statuses.get_mut(&key) else {
            return false;
        };
        status.last_event_ns = Some(event.ts_recv_ns());
        status.state = SubscriptionState::Connected;
        true
    }

    pub fn mark_reconnecting(&mut self, spec: &FeedSpec) -> bool {
        let Some(status) = self.statuses.get_mut(&feed_key(spec)) else {
            return false;
        };
        status.state = SubscriptionState::Reconnecting;
        status.reconnect_count = status.reconnect_count.saturating_add(1);
        true
    }

    pub fn mark_failed(&mut self, spec: &FeedSpec) -> bool {
        let Some(status) = self.statuses.get_mut(&feed_key(spec)) else {
            return false;
        };
        status.state = SubscriptionState::Failed;
        true
    }

    pub fn refresh_staleness(&mut self, now_ns: u64, max_staleness_ns: u64) {
        assert!(max_staleness_ns > 0, "max staleness must be positive");
        for status in self.statuses.values_mut() {
            if matches!(status.state, SubscriptionState::Failed) {
                continue;
            }
            let stale = status
                .last_event_ns
                .is_none_or(|last| now_ns.saturating_sub(last) > max_staleness_ns);
            if stale {
                status.state = SubscriptionState::Stale;
            }
        }
    }
}

fn feed_key(spec: &FeedSpec) -> String {
    format!("{:?}:{}:{:?}", spec.venue, spec.asset, spec.kind)
}

fn feed_spec_for_event(event: &MarketEvent) -> FeedSpec {
    match event {
        MarketEvent::Trade(value) => FeedSpec {
            venue: value.venue,
            asset: value.asset.clone(),
            kind: FeedKind::Trades,
        },
        MarketEvent::BestBidAsk(value) => FeedSpec {
            venue: value.venue,
            asset: value.asset.clone(),
            kind: FeedKind::BestBidAsk,
        },
        MarketEvent::L2Book(value) => FeedSpec {
            venue: value.venue,
            asset: value.asset.clone(),
            kind: FeedKind::L2Book,
        },
        MarketEvent::Candle(value) => FeedSpec {
            venue: value.venue,
            asset: value.asset.clone(),
            kind: FeedKind::Candle {
                interval_ns: value.interval_ns,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AggressorSide, TradeTick};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    fn trade_feed(asset: &str) -> FeedSpec {
        FeedSpec {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            kind: FeedKind::Trades,
        }
    }

    fn trade_event(asset: &str, recv_ns: u64) -> MarketEvent {
        MarketEvent::Trade(TradeTick {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            ts_event_ns: recv_ns,
            ts_recv_ns: recv_ns,
            price: Decimal::ONE,
            quantity: Decimal::ONE,
            aggressor: AggressorSide::Buy,
            sequence: None,
        })
    }

    #[test]
    fn event_updates_only_the_matching_feed() {
        let hype = trade_feed("HYPE");
        let sol = trade_feed("SOL");
        let mut supervisor = SubscriptionSupervisor::new();
        supervisor.apply_derived_feeds([hype.clone(), sol.clone()]);

        assert!(supervisor.observe(&trade_event("HYPE", 100)));
        assert_eq!(
            supervisor.status(&hype).unwrap().state,
            SubscriptionState::Connected
        );
        assert_eq!(supervisor.status(&hype).unwrap().last_event_ns, Some(100));
        assert_eq!(
            supervisor.status(&sol).unwrap().state,
            SubscriptionState::Reconnecting
        );
        assert_eq!(supervisor.status(&sol).unwrap().last_event_ns, None);
        assert_eq!(supervisor.connected_count(), 1);
        assert!(!supervisor.all_connected());

        supervisor.observe(&trade_event("SOL", 101));
        assert!(supervisor.all_connected());
    }

    #[test]
    fn removing_a_derived_feed_removes_its_health_state() {
        let hype = trade_feed("HYPE");
        let sol = trade_feed("SOL");
        let mut supervisor = SubscriptionSupervisor::new();
        supervisor.apply_derived_feeds([hype.clone(), sol.clone()]);
        supervisor.apply_derived_feeds([hype.clone()]);

        assert_eq!(supervisor.feed_count(), 1);
        assert!(supervisor.status(&hype).is_some());
        assert!(supervisor.status(&sol).is_none());
    }

    #[test]
    fn stale_detection_is_feed_scoped_and_fail_closed() {
        let hype = trade_feed("HYPE");
        let sol = trade_feed("SOL");
        let mut supervisor = SubscriptionSupervisor::new();
        supervisor.apply_derived_feeds([hype.clone(), sol.clone()]);
        supervisor.observe(&trade_event("HYPE", 100));

        supervisor.refresh_staleness(150, 100);
        assert_eq!(
            supervisor.status(&hype).unwrap().state,
            SubscriptionState::Connected
        );
        assert_eq!(
            supervisor.status(&sol).unwrap().state,
            SubscriptionState::Stale
        );

        supervisor.refresh_staleness(201, 100);
        assert_eq!(
            supervisor.status(&hype).unwrap().state,
            SubscriptionState::Stale
        );
        assert!(!supervisor.all_connected());
    }
}
