use std::time::Duration;

use tokio::sync::mpsc;

use crate::{MarketDataError, MarketEvent};

use super::subscription::SubscriptionState;

/// Runtime-facing event loop for long-lived market subscriptions.
///
/// Reconnect policy remains owned by the outer runtime. This consumer never
/// treats an exhausted event channel as success: the caller must restart or
/// fail the affected subscription before strategies can open new exposure.
pub struct SubscriptionRuntime {
    reconnect_backoff: Duration,
    state: SubscriptionState,
}

impl SubscriptionRuntime {
    pub fn new() -> Self {
        Self {
            reconnect_backoff: Duration::from_secs(1),
            state: SubscriptionState::Reconnecting,
        }
    }

    pub fn with_reconnect_backoff(reconnect_backoff: Duration) -> Self {
        Self {
            reconnect_backoff,
            state: SubscriptionState::Reconnecting,
        }
    }

    pub fn state(&self) -> SubscriptionState {
        self.state
    }

    pub async fn consume_events<F>(
        &mut self,
        mut receiver: mpsc::Receiver<MarketEvent>,
        mut on_event: F,
    ) -> Result<(), MarketDataError>
    where
        F: FnMut(MarketEvent) -> Result<(), MarketDataError>,
    {
        self.state = SubscriptionState::Connected;

        while let Some(event) = receiver.recv().await {
            if let Err(error) = on_event(event) {
                self.state = SubscriptionState::Degraded;
                return Err(error);
            }
        }

        self.state = SubscriptionState::Reconnecting;
        if !self.reconnect_backoff.is_zero() {
            tokio::time::sleep(self.reconnect_backoff).await;
        }
        Err(MarketDataError::Disconnected(
            "market event channel closed; subscription restart required".into(),
        ))
    }
}

impl Default for SubscriptionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AggressorSide, TradeTick};
    use pg_types::Venue;
    use rust_decimal::Decimal;

    fn trade(recv_ns: u64) -> MarketEvent {
        MarketEvent::Trade(TradeTick {
            venue: Venue::Hyperliquid,
            asset: "HYPE".into(),
            ts_event_ns: recv_ns,
            ts_recv_ns: recv_ns,
            price: Decimal::ONE,
            quantity: Decimal::ONE,
            aggressor: AggressorSide::Buy,
            sequence: None,
        })
    }

    #[tokio::test]
    async fn forwards_events_to_the_runtime_handler() {
        let (sender, receiver) = mpsc::channel(2);
        sender.send(trade(42)).await.unwrap();
        drop(sender);

        let mut seen = Vec::new();
        let mut runtime = SubscriptionRuntime::with_reconnect_backoff(Duration::ZERO);
        let result = runtime
            .consume_events(receiver, |event| {
                seen.push(event.ts_recv_ns());
                Ok(())
            })
            .await;

        assert_eq!(seen, vec![42]);
        assert!(matches!(result, Err(MarketDataError::Disconnected(_))));
        assert_eq!(runtime.state(), SubscriptionState::Reconnecting);
    }

    #[tokio::test]
    async fn handler_failure_degrades_the_subscription() {
        let (sender, receiver) = mpsc::channel(1);
        sender.send(trade(42)).await.unwrap();

        let mut runtime = SubscriptionRuntime::with_reconnect_backoff(Duration::ZERO);
        let result = runtime
            .consume_events(receiver, |_| {
                Err(MarketDataError::Conversion("handler failed".into()))
            })
            .await;

        assert!(matches!(result, Err(MarketDataError::Conversion(_))));
        assert_eq!(runtime.state(), SubscriptionState::Degraded);
    }
}
