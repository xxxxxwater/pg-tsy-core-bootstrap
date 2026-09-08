use std::time::Duration;

use tokio::sync::mpsc;

use crate::{MarketEvent, MarketDataError};

use super::subscription::SubscriptionState;

/// Runtime-facing control loop for long lived market subscriptions.
///
/// The supervisor intentionally keeps reconnect policy outside venue adapters:
/// adapters only produce normalized MarketEvent values, while runtime owns
/// lifecycle, health and recovery decisions.
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

    pub fn state(&self) -> SubscriptionState {
        self.state
    }

    pub async fn consume_events(
        &mut self,
        mut receiver: mpsc::Receiver<MarketEvent>,
    ) -> Result<(), MarketDataError> {
        self.state = SubscriptionState::Connected;

        while let Some(_event) = receiver.recv().await {
            // Future stages connect here:
            // MarketEvent -> LiveFeatureEngine -> PolicyEngine -> Risk.
        }

        self.state = SubscriptionState::Reconnecting;
        tokio::time::sleep(self.reconnect_backoff).await;
        Ok(())
    }
}

impl Default for SubscriptionRuntime {
    fn default() -> Self {
        Self::new()
    }
}
