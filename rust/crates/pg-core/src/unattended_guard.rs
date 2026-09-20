//! Conservative admission and policy-order guards for the real-venue daemon.
//! A healthy reconciliation does not by itself heal an independent topology fault.
//! A missing venue order lookup is not proof that an attempted submit is terminal.
use pg_oms::OrderRecord;
use pg_types::Venue;
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct EntryGuard {
    topology_blocker: Option<String>,
    reconcile_clean: bool,
}

impl EntryGuard {
    pub(crate) fn new(startup_clean: bool) -> Self {
        Self {
            topology_blocker: None,
            reconcile_clean: startup_clean,
        }
    }

    pub(crate) fn begin_topology_change(&mut self) {
        self.topology_blocker = Some("topology change pending validation".into());
    }

    pub(crate) fn topology_validated(&mut self) {
        self.topology_blocker = None;
    }

    pub(crate) fn topology_failed(&mut self, reason: impl Into<String>) {
        self.topology_blocker = Some(reason.into());
    }

    pub(crate) fn reconcile_result(&mut self, clean: bool) {
        self.reconcile_clean = clean;
    }

    pub(crate) fn topology_blocker(&self) -> Option<&str> {
        self.topology_blocker.as_deref()
    }

    pub(crate) fn ready(&self, checklist_ready: bool) -> bool {
        self.topology_blocker.is_none() && self.reconcile_clean && checklist_ready
    }

    pub(crate) fn may_increase(
        &self,
        operator_started: bool,
        checklist_ready: bool,
        feeds_connected: bool,
    ) -> bool {
        operator_started && feeds_connected && self.ready(checklist_ready)
    }
}

/// Called only with the last *cleanly reconciled* order snapshot. Any active
/// durable strategy order blocks a new order, including after a process restart.
/// A remembered attempt clears only when that same client ID is present in a
/// cleanly reconciled, terminal, correctly owned durable record. Neither a
/// missing lookup nor a missing local record is terminal evidence.
pub(crate) fn policy_order_busy(
    strategy_id: &str,
    orders: &BTreeMap<String, OrderRecord>,
    attempted: &mut BTreeMap<String, (Venue, String)>,
) -> bool {
    if orders
        .values()
        .any(|order| order.owner_strategy_id == strategy_id && !order.is_terminal())
    {
        return true;
    }

    let Some((venue, client_id)) = attempted.get(strategy_id).cloned() else {
        return false;
    };
    match orders.get(&client_id) {
        Some(record)
            if record.venue == venue
                && record.owner_strategy_id == strategy_id
                && record.is_terminal() =>
        {
            attempted.remove(strategy_id);
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_oms::OrderState;
    use pg_types::{ExposureEffect, OrderIntent, Side};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn order(strategy: &str, state: OrderState) -> OrderRecord {
        let intent = OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: strategy.into(),
            asset: "HYPE".into(),
            venue: Venue::Hyperliquid,
            side: Side::Buy,
            quantity: Decimal::ONE,
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        };
        let mut record = OrderRecord::from_intent(&intent);
        record.state = state;
        record
    }

    #[test]
    fn topology_failure_cannot_be_cleared_by_clean_reconcile_or_healthy_feeds() {
        let mut gate = EntryGuard::new(true);
        assert!(gate.may_increase(true, true, true));
        gate.begin_topology_change();
        assert!(!gate.may_increase(true, true, true));
        gate.topology_failed("unregistered instrument");
        gate.reconcile_result(true);
        assert!(!gate.ready(true));
        assert_eq!(gate.topology_blocker(), Some("unregistered instrument"));
        gate.topology_validated();
        assert!(gate.may_increase(true, true, true));
    }

    #[test]
    fn reconcile_and_operator_and_feed_gates_are_independent() {
        let mut gate = EntryGuard::new(false);
        assert!(!gate.may_increase(true, true, true));
        gate.reconcile_result(true);
        assert!(!gate.may_increase(false, true, true));
        assert!(!gate.may_increase(true, false, true));
        assert!(!gate.may_increase(true, true, false));
        assert!(gate.may_increase(true, true, true));
        gate.reconcile_result(false);
        assert!(!gate.may_increase(true, true, true));
    }

    #[test]
    fn restart_with_open_order_keeps_strategy_busy() {
        let record = order("alpha", OrderState::PartiallyFilled);
        let mut orders = BTreeMap::new();
        orders.insert(record.client_order_id.clone(), record);
        assert!(policy_order_busy("alpha", &orders, &mut BTreeMap::new()));
        assert!(!policy_order_busy("beta", &orders, &mut BTreeMap::new()));
    }

    #[test]
    fn lost_ack_or_missing_lookup_does_not_allow_resubmit() {
        let mut attempted = BTreeMap::from([(
            "alpha".into(),
            (Venue::Hyperliquid, "potentially-live".into()),
        )]);
        assert!(policy_order_busy("alpha", &BTreeMap::new(), &mut attempted));
        assert!(attempted.contains_key("alpha"));
        let foreign = order("other", OrderState::Filled);
        let orders = BTreeMap::from([("potentially-live".into(), foreign)]);
        assert!(policy_order_busy("alpha", &orders, &mut attempted));
    }

    #[test]
    fn only_reconciled_terminal_matching_order_clears_attempt() {
        let mut record = order("alpha", OrderState::Open);
        let client_id = record.client_order_id.clone();
        let mut attempted = BTreeMap::from([(
            "alpha".into(),
            (Venue::Hyperliquid, client_id.clone()),
        )]);
        let mut orders = BTreeMap::from([(client_id.clone(), record.clone())]);
        assert!(policy_order_busy("alpha", &orders, &mut attempted));
        record.state = OrderState::Filled;
        orders.insert(client_id, record);
        assert!(!policy_order_busy("alpha", &orders, &mut attempted));
        assert!(!attempted.contains_key("alpha"));
    }
}
