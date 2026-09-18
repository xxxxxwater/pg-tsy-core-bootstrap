use pg_execution::{VenueOrderSnapshot, VenueOrderState};
use pg_oms::{OrderRecord, OrderState};
use pg_types::{AssetKey, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Ownership {
    Strategy(String),
    Manual,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenuePosition {
    pub venue: Venue,
    pub asset: String,
    pub quantity: Decimal,
    pub ownership: Ownership,
}

impl VenuePosition {
    pub fn key(&self) -> AssetKey {
        AssetKey::new(self.venue, self.asset.clone())
    }
}

/// Conservative legacy gate: any unknown position anywhere blocks global expansion.
pub fn may_open_new_exposure(positions: &[VenuePosition]) -> bool {
    !positions.iter().any(|p| p.ownership == Ownership::Unknown)
}

/// Production gate used by strategy/risk: unknown ownership only freezes the
/// affected venue+asset. A known Manual position is allowed to coexist because
/// it is not attributed to the strategy and must never be reduced implicitly.
pub fn may_open_new_exposure_for(positions: &[VenuePosition], venue: Venue, asset: &str) -> bool {
    !positions.iter().any(|position| {
        position.venue == venue
            && position.asset == asset
            && position.ownership == Ownership::Unknown
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconcileIssue {
    UnknownPositionOwnership {
        key: AssetKey,
        quantity: Decimal,
    },
    LocalOrderMissingAtVenue {
        key: AssetKey,
        client_order_id: String,
        local_state: OrderState,
    },
    VenueOrderMissingLocally {
        key: AssetKey,
        client_order_id: Option<String>,
        venue_order_id: String,
    },
    FilledQuantityMismatch {
        key: AssetKey,
        client_order_id: String,
        local_filled: Decimal,
        venue_filled: Decimal,
    },
    /// A matching cumulative fill never resolves an UNKNOWN venue/local state.
    UnresolvedOrderState {
        key: AssetKey,
        client_order_id: String,
    },
    /// The same client ID cannot silently stand for a different instrument,
    /// side, size or venue order ID. Legacy records without a side fail closed.
    OrderContractMismatch {
        key: AssetKey,
        client_order_id: String,
        venue_asset: String,
    },
}

impl ReconcileIssue {
    pub fn key(&self) -> &AssetKey {
        match self {
            Self::UnknownPositionOwnership { key, .. }
            | Self::LocalOrderMissingAtVenue { key, .. }
            | Self::VenueOrderMissingLocally { key, .. }
            | Self::FilledQuantityMismatch { key, .. }
            | Self::UnresolvedOrderState { key, .. }
            | Self::OrderContractMismatch { key, .. } => key,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub issues: Vec<ReconcileIssue>,
    pub safe_hold_assets: BTreeSet<AssetKey>,
}

impl ReconcileReport {
    pub fn clean(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn blocks(&self, venue: Venue, asset: &str) -> bool {
        self.safe_hold_assets
            .contains(&AssetKey::new(venue, asset.to_owned()))
    }
}

pub fn reconcile(
    venue: Venue,
    local_orders: &[OrderRecord],
    venue_orders: &[VenueOrderSnapshot],
    positions: &[VenuePosition],
) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    for position in positions.iter().filter(|position| position.venue == venue) {
        if position.ownership == Ownership::Unknown && position.quantity != Decimal::ZERO {
            push_issue(
                &mut report,
                ReconcileIssue::UnknownPositionOwnership {
                    key: position.key(),
                    quantity: position.quantity,
                },
            );
        }
    }

    let venue_by_client: BTreeMap<&str, &VenueOrderSnapshot> = venue_orders
        .iter()
        .filter_map(|order| {
            order
                .client_order_id
                .as_deref()
                .map(|client_id| (client_id, order))
        })
        .collect();

    for local in local_orders
        .iter()
        .filter(|order| order.venue == venue && !order.is_terminal())
    {
        let key = AssetKey::new(local.venue, local.asset.clone());
        match venue_by_client.get(local.client_order_id.as_str()) {
            Some(remote) => {
                if remote.filled_quantity != local.filled_quantity {
                    push_issue(
                        &mut report,
                        ReconcileIssue::FilledQuantityMismatch {
                            key: key.clone(),
                            client_order_id: local.client_order_id.clone(),
                            local_filled: local.filled_quantity,
                            venue_filled: remote.filled_quantity,
                        },
                    );
                }
                if local.state == OrderState::Unknown || remote.state == VenueOrderState::Unknown {
                    push_issue(
                        &mut report,
                        ReconcileIssue::UnresolvedOrderState {
                            key: key.clone(),
                            client_order_id: local.client_order_id.clone(),
                        },
                    );
                }
                if remote.asset != local.asset
                    || local.side != Some(remote.side)
                    || remote.requested_quantity != local.requested_quantity
                    || local
                        .venue_order_id
                        .as_ref()
                        .is_some_and(|id| id != &remote.venue_order_id)
                    || remote.venue_order_id.is_empty()
                {
                    push_issue(
                        &mut report,
                        ReconcileIssue::OrderContractMismatch {
                            key: key.clone(),
                            client_order_id: local.client_order_id.clone(),
                            venue_asset: remote.asset.clone(),
                        },
                    );
                    // A mismatched remote asset is also untrusted: freeze both
                    // instruments until ownership is independently verified.
                    if remote.asset != local.asset {
                        push_issue(
                            &mut report,
                            ReconcileIssue::OrderContractMismatch {
                                key: AssetKey::new(venue, remote.asset.clone()),
                                client_order_id: local.client_order_id.clone(),
                                venue_asset: remote.asset.clone(),
                            },
                        );
                    }
                }
            }
            None => push_issue(
                &mut report,
                ReconcileIssue::LocalOrderMissingAtVenue {
                    key,
                    client_order_id: local.client_order_id.clone(),
                    local_state: local.state,
                },
            ),
        }
    }

    let local_by_client: BTreeMap<&str, &OrderRecord> = local_orders
        .iter()
        .filter(|order| order.venue == venue)
        .map(|order| (order.client_order_id.as_str(), order))
        .collect();

    for remote in venue_orders {
        let known = remote
            .client_order_id
            .as_deref()
            .and_then(|client_id| local_by_client.get(client_id))
            .is_some();
        if !known {
            push_issue(
                &mut report,
                ReconcileIssue::VenueOrderMissingLocally {
                    key: AssetKey::new(venue, remote.asset.clone()),
                    client_order_id: remote.client_order_id.clone(),
                    venue_order_id: remote.venue_order_id.clone(),
                },
            );
        }
    }

    report
}

fn push_issue(report: &mut ReconcileReport, issue: ReconcileIssue) {
    report.safe_hold_assets.insert(issue.key().clone());
    report.issues.push(issue);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::{ExposureEffect, OrderIntent, Side};
    use uuid::Uuid;

    fn intent(asset: &str) -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: "s1".into(),
            asset: asset.into(),
            venue: Venue::Hyperliquid,
            side: Side::Buy,
            quantity: Decimal::from(10),
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        }
    }

    #[test]
    fn manual_position_does_not_block_other_or_same_known_asset() {
        let positions = vec![VenuePosition {
            venue: Venue::Hyperliquid,
            asset: "BTC".into(),
            quantity: Decimal::ONE,
            ownership: Ownership::Manual,
        }];
        assert!(may_open_new_exposure_for(
            &positions,
            Venue::Hyperliquid,
            "BTC"
        ));
        assert!(may_open_new_exposure_for(
            &positions,
            Venue::Hyperliquid,
            "HYPE"
        ));
    }

    #[test]
    fn unknown_position_only_blocks_its_asset() {
        let positions = vec![VenuePosition {
            venue: Venue::Hyperliquid,
            asset: "BTC".into(),
            quantity: Decimal::ONE,
            ownership: Ownership::Unknown,
        }];
        assert!(!may_open_new_exposure_for(
            &positions,
            Venue::Hyperliquid,
            "BTC"
        ));
        assert!(may_open_new_exposure_for(
            &positions,
            Venue::Hyperliquid,
            "HYPE"
        ));
    }

    fn open_order_fixture() -> (OrderRecord, VenueOrderSnapshot) {
        let intent = intent("HYPE");
        let mut local = OrderRecord::from_intent(&intent);
        local.apply(pg_oms::OrderEvent::SubmitRequested).unwrap();
        local.accept("42").unwrap();
        let remote = VenueOrderSnapshot {
            venue_order_id: "42".into(),
            client_order_id: Some(intent.client_order_id()),
            asset: "HYPE".into(),
            side: Side::Buy,
            requested_quantity: Decimal::from(10),
            filled_quantity: Decimal::ZERO,
            limit_price: None,
            state: VenueOrderState::Open,
        };
        (local, remote)
    }

    #[test]
    fn fill_drift_enters_asset_safe_hold() {
        let (mut local, mut remote) = open_order_fixture();
        local.apply_fill(Decimal::from(3)).unwrap();
        remote.filled_quantity = Decimal::from(5);
        remote.state = VenueOrderState::PartiallyFilled;
        let report = reconcile(Venue::Hyperliquid, &[local], &[remote], &[]);
        assert!(report.blocks(Venue::Hyperliquid, "HYPE"));
        assert!(matches!(
            report.issues.first(),
            Some(ReconcileIssue::FilledQuantityMismatch { .. })
        ));
    }

    #[test]
    fn unknown_order_state_is_never_clean_even_if_fills_match() {
        let (local, mut remote) = open_order_fixture();
        assert!(reconcile(Venue::Hyperliquid, &[local.clone()], &[remote.clone()], &[]).clean());
        remote.state = VenueOrderState::Unknown;
        let report = reconcile(Venue::Hyperliquid, &[local.clone()], &[remote], &[]);
        assert!(report.blocks(Venue::Hyperliquid, "HYPE"));
        assert!(matches!(
            report.issues.first(),
            Some(ReconcileIssue::UnresolvedOrderState { .. })
        ));
        let mut local = local;
        local.state = OrderState::Unknown;
        let mut known_remote = open_order_fixture().1;
        known_remote.client_order_id = Some(local.client_order_id.clone());
        assert!(reconcile(Venue::Hyperliquid, &[local], &[known_remote], &[]).blocks(Venue::Hyperliquid, "HYPE"));
    }

    #[test]
    fn mismatched_remote_identity_freezes_both_assets() {
        let (local, mut remote) = open_order_fixture();
        remote.asset = "BTC".into();
        let report = reconcile(Venue::Hyperliquid, &[local], &[remote], &[]);
        assert!(report.blocks(Venue::Hyperliquid, "HYPE"));
        assert!(report.blocks(Venue::Hyperliquid, "BTC"));
        assert!(matches!(report.issues.first(), Some(ReconcileIssue::OrderContractMismatch { .. })));
    }

    #[test]
    fn side_quantity_and_exchange_order_id_mismatch_hold() {
        let (local, mut remote) = open_order_fixture();
        remote.side = Side::Sell;
        assert!(reconcile(Venue::Hyperliquid, &[local.clone()], &[remote.clone()], &[]).blocks(Venue::Hyperliquid, "HYPE"));
        remote.side = Side::Buy;
        remote.requested_quantity += Decimal::ONE;
        assert!(reconcile(Venue::Hyperliquid, &[local.clone()], &[remote.clone()], &[]).blocks(Venue::Hyperliquid, "HYPE"));
        remote.requested_quantity -= Decimal::ONE;
        remote.venue_order_id = "999".into();
        assert!(reconcile(Venue::Hyperliquid, &[local.clone()], &[remote], &[]).blocks(Venue::Hyperliquid, "HYPE"));
        let mut missing_side = local;
        missing_side.side = None;
        let good_remote = open_order_fixture().1;
        assert!(reconcile(Venue::Hyperliquid, &[missing_side], &[good_remote], &[]).blocks(Venue::Hyperliquid, "HYPE"));
    }
}
