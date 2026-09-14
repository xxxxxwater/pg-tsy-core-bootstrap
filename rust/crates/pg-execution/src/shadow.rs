//! Simulated, in-process venue adapter.
//!
//! The production path is
//!
//! ```text
//! strategy decision -> risk -> OMS -> journal -> execution adapter -> venue
//! ```
//!
//! Without a registered adapter there is nothing to journal against, which is why
//! the shadow daemon historically stopped at "log the intent". `ShadowExecutionAdapter`
//! closes that gap **without any network side effect**: it implements the same
//! `ExecutionAdapter` contract as the Hyperliquid and IBKR adapters, so the risk gate,
//! OMS transitions and journal-before-dispatch ordering are genuinely exercised while
//! the counterparty is a local book.
//!
//! It is deliberately conservative:
//!
//! * `ShadowFillMode::Rest` (the default) only ever acknowledges orders: nothing fills
//!   and no position is created.
//! * `ShadowFillMode::ImmediateFill` fills on acknowledgement, which is what makes the
//!   position/exit half of a strategy observable before a real venue is wired.
//! * reduce-only intents are rejected unless they strictly shrink an existing
//!   opposite-signed position, mirroring the IBKR software guard.
//!
//! This adapter must never be registered for a real venue in `PG_RUN_MODE=live`.

use crate::{
    ExecutionAdapter, ExecutionError, OrderLocator, VenueOrderAck, VenueOrderSnapshot,
    VenueOrderState, VenuePositionSnapshot,
};
use async_trait::async_trait;
use pg_types::{ExposureEffect, OrderIntent, Side, Venue};
use rust_decimal::Decimal;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// What happens to an order once the shadow venue acknowledges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShadowFillMode {
    /// Acknowledge and leave the order resting. No fill, no position.
    #[default]
    Rest,
    /// Acknowledge and fill the whole order immediately.
    ImmediateFill,
}

#[derive(Debug, Clone)]
pub struct ShadowAdapterConfig {
    pub venue: Venue,
    pub fill_mode: ShadowFillMode,
}

impl ShadowAdapterConfig {
    pub fn new(venue: Venue) -> Self {
        Self {
            venue,
            fill_mode: ShadowFillMode::Rest,
        }
    }

    pub fn with_fill_mode(mut self, fill_mode: ShadowFillMode) -> Self {
        self.fill_mode = fill_mode;
        self
    }
}

/// `rust_decimal::Decimal` has no `signum`; three-way comparison is what the
/// reduce-only guard actually needs.
fn sign_of(value: Decimal) -> i32 {
    if value > Decimal::ZERO {
        1
    } else if value < Decimal::ZERO {
        -1
    } else {
        0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShadowPosition {
    pub net_quantity: Decimal,
    pub average_entry_price: Option<Decimal>,
    pub filled_entries: u32,
}

#[derive(Debug, Default)]
struct ShadowBook {
    orders: Vec<VenueOrderSnapshot>,
    positions: BTreeMap<String, ShadowPosition>,
    next_order_id: u64,
}

impl ShadowBook {
    fn position(&self, asset: &str) -> ShadowPosition {
        self.positions.get(asset).cloned().unwrap_or_default()
    }

    /// Signed quantity change implied by an order.
    fn signed(quantity: Decimal, side: Side) -> Decimal {
        match side {
            Side::Buy => quantity,
            Side::Sell => -quantity,
        }
    }

    fn apply_fill(&mut self, asset: &str, side: Side, quantity: Decimal, price: Decimal) {
        let mut position = self.position(asset);
        let delta = Self::signed(quantity, side);
        let previous = position.net_quantity;
        let resulting = previous + delta;

        let price = if price.is_zero() { None } else { Some(price) };
        position.average_entry_price = match (previous.is_zero(), price) {
            // Opening or flipping: the new average is the fill price.
            (true, Some(price)) => Some(price),
            (false, Some(price)) if sign_of(previous) == sign_of(delta) => {
                let previous_notional =
                    previous.abs() * position.average_entry_price.unwrap_or(price);
                let added_notional = delta.abs() * price;
                Some((previous_notional + added_notional) / resulting.abs())
            }
            // Reducing: keep the existing average until the position flips.
            (false, _) if !resulting.is_zero() && sign_of(previous) != sign_of(resulting) => price,
            _ => position.average_entry_price,
        };
        position.filled_entries = position.filled_entries.saturating_add(1);
        position.net_quantity = resulting;
        if resulting.is_zero() {
            position.average_entry_price = None;
        }
        self.positions.insert(asset.to_string(), position);
    }
}

/// In-memory venue stand-in. Cloneable; clones share the same book.
#[derive(Debug, Clone)]
pub struct ShadowExecutionAdapter {
    config: ShadowAdapterConfig,
    book: Arc<Mutex<ShadowBook>>,
    reference_price: Arc<Mutex<Option<Decimal>>>,
}

impl ShadowExecutionAdapter {
    pub fn new(config: ShadowAdapterConfig) -> Self {
        Self {
            config,
            book: Arc::new(Mutex::new(ShadowBook::default())),
            reference_price: Arc::new(Mutex::new(None)),
        }
    }

    pub fn venue(&self) -> Venue {
        self.config.venue
    }

    pub fn fill_mode(&self) -> ShadowFillMode {
        self.config.fill_mode
    }

    /// Latest observed market price, used to price simulated fills and to mark
    /// positions to market. The daemon feeds this from normalized market events.
    pub fn set_reference_price(&self, price: Decimal) {
        if price > Decimal::ZERO {
            *self
                .reference_price
                .lock()
                .expect("shadow reference price lock poisoned") = Some(price);
        }
    }

    pub fn reference_price(&self) -> Option<Decimal> {
        *self
            .reference_price
            .lock()
            .expect("shadow reference price lock poisoned")
    }

    pub fn position(&self, asset: &str) -> ShadowPosition {
        self.book
            .lock()
            .expect("shadow book lock poisoned")
            .position(asset)
    }

    pub fn filled_quantity(&self, client_order_id: &str) -> Decimal {
        self.book
            .lock()
            .expect("shadow book lock poisoned")
            .orders
            .iter()
            .filter(|order| order.client_order_id.as_deref() == Some(client_order_id))
            .map(|order| order.filled_quantity)
            .sum()
    }

    /// Whether an order for this client id is still working at the shadow venue.
    pub fn is_client_order_live(&self, client_order_id: &str) -> bool {
        self.order_by_client_id(client_order_id)
            .is_some_and(|order| {
                matches!(
                    order.state,
                    VenueOrderState::Open | VenueOrderState::PartiallyFilled
                )
            })
    }

    pub fn resting_order_count(&self) -> usize {
        self.book
            .lock()
            .expect("shadow book lock poisoned")
            .orders
            .iter()
            .filter(|order| {
                matches!(
                    order.state,
                    VenueOrderState::Open | VenueOrderState::PartiallyFilled
                )
            })
            .count()
    }

    fn order_by_client_id(&self, client_order_id: &str) -> Option<VenueOrderSnapshot> {
        self.book
            .lock()
            .expect("shadow book lock poisoned")
            .orders
            .iter()
            .rev()
            .find(|order| order.client_order_id.as_deref() == Some(client_order_id))
            .cloned()
    }

    /// Reject a reduce-only order that would increase exposure or cross through flat.
    ///
    /// This is a software guard with the same timing-window caveat the IBKR adapter
    /// documents: it is not a venue-native atomic reduce-only primitive.
    fn guard_reduce_only(
        &self,
        asset: &str,
        side: Side,
        quantity: Decimal,
    ) -> Result<(), ExecutionError> {
        if quantity <= Decimal::ZERO {
            return Err(ExecutionError::Rejected(
                "reduce-only quantity must be positive".into(),
            ));
        }
        let current = self.position(asset).net_quantity;
        if current.is_zero() {
            return Err(ExecutionError::Rejected(format!(
                "reduce-only order for {asset} rejected: position is flat"
            )));
        }
        let delta = ShadowBook::signed(quantity, side);
        if sign_of(current) == sign_of(delta) {
            return Err(ExecutionError::Rejected(format!(
                "reduce-only order for {asset} rejected: would increase exposure"
            )));
        }
        if sign_of(current + delta) == sign_of(delta) {
            return Err(ExecutionError::Rejected(format!(
                "reduce-only order for {asset} rejected: would cross through flat"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ExecutionAdapter for ShadowExecutionAdapter {
    async fn submit(&self, intent: &OrderIntent) -> Result<VenueOrderAck, ExecutionError> {
        if intent.venue != self.config.venue {
            return Err(ExecutionError::Unsupported(format!(
                "shadow adapter for {:?} received an intent for {:?}",
                self.config.venue, intent.venue
            )));
        }
        if intent.quantity <= Decimal::ZERO {
            return Err(ExecutionError::Rejected(
                "order quantity must be positive".into(),
            ));
        }
        if intent.effect == ExposureEffect::ReduceOnly {
            self.guard_reduce_only(&intent.asset, intent.side, intent.quantity)?;
        }

        let client_order_id = intent.client_order_id();
        let mut book = self.book.lock().expect("shadow book lock poisoned");

        // Idempotency: a replayed intent must adopt the existing order, never duplicate it.
        if let Some(existing) = book
            .orders
            .iter()
            .find(|order| order.client_order_id.as_deref() == Some(client_order_id.as_str()))
        {
            return Ok(VenueOrderAck {
                venue_order_id: existing.venue_order_id.clone(),
                client_order_id,
            });
        }

        book.next_order_id = book.next_order_id.saturating_add(1);
        let venue_order_id = format!("shadow-{}", book.next_order_id);

        let fills_immediately = self.config.fill_mode == ShadowFillMode::ImmediateFill;
        let (state, filled_quantity) = if fills_immediately {
            (VenueOrderState::Filled, intent.quantity)
        } else {
            (VenueOrderState::Open, Decimal::ZERO)
        };
        book.orders.push(VenueOrderSnapshot {
            venue_order_id: venue_order_id.clone(),
            client_order_id: Some(client_order_id.clone()),
            asset: intent.asset.clone(),
            side: intent.side,
            requested_quantity: intent.quantity,
            filled_quantity,
            limit_price: intent.limit_price,
            state,
        });

        if fills_immediately {
            let price = intent
                .limit_price
                .or(self.reference_price())
                .unwrap_or(Decimal::ZERO);
            book.apply_fill(&intent.asset, intent.side, intent.quantity, price);
        }

        Ok(VenueOrderAck {
            venue_order_id,
            client_order_id,
        })
    }

    async fn cancel(&self, order: OrderLocator<'_>) -> Result<(), ExecutionError> {
        let mut book = self.book.lock().expect("shadow book lock poisoned");
        let Some(existing) = book.orders.iter_mut().find(|candidate| {
            candidate.client_order_id.as_deref() == Some(order.client_order_id)
                || order
                    .venue_order_id
                    .is_some_and(|id| candidate.venue_order_id == id)
        }) else {
            return Err(ExecutionError::Rejected(format!(
                "unknown shadow order {}",
                order.client_order_id
            )));
        };
        // Terminal states are not cancellable, and pretending otherwise would hide a
        // reconciliation problem.
        if matches!(
            existing.state,
            VenueOrderState::Open | VenueOrderState::PartiallyFilled
        ) {
            existing.state = VenueOrderState::Canceled;
            return Ok(());
        }
        Err(ExecutionError::Rejected(format!(
            "shadow order {} is already {:?}",
            order.client_order_id, existing.state
        )))
    }

    async fn open_orders(&self) -> Result<Vec<VenueOrderSnapshot>, ExecutionError> {
        Ok(self
            .book
            .lock()
            .expect("shadow book lock poisoned")
            .orders
            .iter()
            .filter(|order| {
                matches!(
                    order.state,
                    VenueOrderState::Open | VenueOrderState::PartiallyFilled
                )
            })
            .cloned()
            .collect())
    }

    async fn positions(&self) -> Result<Vec<VenuePositionSnapshot>, ExecutionError> {
        Ok(self
            .book
            .lock()
            .expect("shadow book lock poisoned")
            .positions
            .iter()
            .filter(|(_, position)| !position.net_quantity.is_zero())
            .map(|(asset, position)| VenuePositionSnapshot {
                asset: asset.clone(),
                quantity: position.net_quantity,
            })
            .collect())
    }

    /// Searches every order, not only resting ones, so a fast simulated fill stays
    /// discoverable. This mirrors the IBKR open -> completed -> executions recovery.
    async fn find_order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<VenueOrderSnapshot>, ExecutionError> {
        Ok(self.order_by_client_id(client_order_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Small helper so tests read like the domain values they assert on.
    fn d(value: i64) -> Decimal {
        Decimal::from(value)
    }

    fn intent(asset: &str, side: Side, quantity: Decimal, effect: ExposureEffect) -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: "test-strategy".into(),
            asset: asset.into(),
            venue: Venue::Hyperliquid,
            side,
            quantity,
            limit_price: None,
            effect,
            source_signal_id: None,
        }
    }

    fn adapter(mode: ShadowFillMode) -> ShadowExecutionAdapter {
        ShadowExecutionAdapter::new(
            ShadowAdapterConfig::new(Venue::Hyperliquid).with_fill_mode(mode),
        )
    }

    #[tokio::test]
    async fn rest_mode_acknowledges_without_creating_a_position() {
        let adapter = adapter(ShadowFillMode::Rest);
        let ack = adapter
            .submit(&intent("HYPE", Side::Buy, d(2), ExposureEffect::Increase))
            .await
            .unwrap();
        assert!(ack.venue_order_id.starts_with("shadow-"));
        assert_eq!(adapter.position("HYPE").net_quantity, Decimal::ZERO);
        assert_eq!(adapter.open_orders().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn immediate_fill_updates_the_simulated_position() {
        let adapter = adapter(ShadowFillMode::ImmediateFill);
        adapter.set_reference_price(d(10));
        adapter
            .submit(&intent("HYPE", Side::Buy, d(2), ExposureEffect::Increase))
            .await
            .unwrap();
        let position = adapter.position("HYPE");
        assert_eq!(position.net_quantity, d(2));
        assert_eq!(position.average_entry_price, Some(d(10)));
        assert_eq!(position.filled_entries, 1);
        // A filled order is no longer resting.
        assert_eq!(adapter.open_orders().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn replayed_intent_is_adopted_not_duplicated() {
        let adapter = adapter(ShadowFillMode::Rest);
        let intent = intent("HYPE", Side::Buy, d(1), ExposureEffect::Increase);
        let first = adapter.submit(&intent).await.unwrap();
        let second = adapter.submit(&intent).await.unwrap();
        assert_eq!(first.venue_order_id, second.venue_order_id);
        assert_eq!(adapter.open_orders().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fast_fill_remains_discoverable_by_client_id() {
        let adapter = adapter(ShadowFillMode::ImmediateFill);
        let intent = intent("HYPE", Side::Buy, d(1), ExposureEffect::Increase);
        let ack = adapter.submit(&intent).await.unwrap();
        assert_eq!(adapter.open_orders().await.unwrap().len(), 0);
        let found = adapter
            .find_order_by_client_id(&ack.client_order_id)
            .await
            .unwrap()
            .expect("a filled order must still be discoverable");
        assert_eq!(found.state, VenueOrderState::Filled);
    }

    #[tokio::test]
    async fn reduce_only_on_flat_position_is_rejected() {
        let adapter = adapter(ShadowFillMode::ImmediateFill);
        let error = adapter
            .submit(&intent(
                "HYPE",
                Side::Sell,
                d(1),
                ExposureEffect::ReduceOnly,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Rejected(_)), "{error:?}");
    }

    #[tokio::test]
    async fn reduce_only_wrong_direction_is_rejected() {
        let adapter = adapter(ShadowFillMode::ImmediateFill);
        adapter.set_reference_price(d(10));
        adapter
            .submit(&intent("HYPE", Side::Buy, d(2), ExposureEffect::Increase))
            .await
            .unwrap();
        let error = adapter
            .submit(&intent("HYPE", Side::Buy, d(1), ExposureEffect::ReduceOnly))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Rejected(_)), "{error:?}");
    }

    #[tokio::test]
    async fn reduce_only_that_would_cross_through_flat_is_rejected() {
        let adapter = adapter(ShadowFillMode::ImmediateFill);
        adapter.set_reference_price(d(10));
        adapter
            .submit(&intent("HYPE", Side::Buy, d(2), ExposureEffect::Increase))
            .await
            .unwrap();
        let error = adapter
            .submit(&intent(
                "HYPE",
                Side::Sell,
                d(3),
                ExposureEffect::ReduceOnly,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Rejected(_)), "{error:?}");

        // Exactly closing the position is allowed.
        adapter
            .submit(&intent(
                "HYPE",
                Side::Sell,
                d(2),
                ExposureEffect::ReduceOnly,
            ))
            .await
            .unwrap();
        assert_eq!(adapter.position("HYPE").net_quantity, Decimal::ZERO);
    }

    #[tokio::test]
    async fn cancel_only_accepts_resting_orders() {
        let adapter = adapter(ShadowFillMode::Rest);
        let ack = adapter
            .submit(&intent("HYPE", Side::Buy, d(1), ExposureEffect::Increase))
            .await
            .unwrap();
        adapter
            .cancel(OrderLocator {
                asset: "HYPE",
                venue_order_id: Some(&ack.venue_order_id),
                client_order_id: &ack.client_order_id,
            })
            .await
            .unwrap();
        assert_eq!(adapter.open_orders().await.unwrap().len(), 0);

        let error = adapter
            .cancel(OrderLocator {
                asset: "HYPE",
                venue_order_id: None,
                client_order_id: &ack.client_order_id,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Rejected(_)), "{error:?}");
    }

    #[tokio::test]
    async fn wrong_venue_is_refused() {
        let adapter = adapter(ShadowFillMode::Rest);
        let mut foreign = intent("AAPL", Side::Buy, d(1), ExposureEffect::Increase);
        foreign.venue = Venue::InteractiveBrokers;
        let error = adapter.submit(&foreign).await.unwrap_err();
        assert!(matches!(error, ExecutionError::Unsupported(_)), "{error:?}");
    }
}
