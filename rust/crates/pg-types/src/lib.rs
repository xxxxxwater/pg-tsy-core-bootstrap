use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Venue {
    BinancePm,
    Hyperliquid,
    #[serde(rename = "IBKR")]
    InteractiveBrokers,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetKey {
    pub venue: Venue,
    pub asset: String,
}

impl AssetKey {
    pub fn new(venue: Venue, asset: impl Into<String>) -> Self {
        Self {
            venue,
            asset: asset.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub schema_version: String,
    pub signal_id: String,
    pub alpha_id: String,
    pub asset: String,
    pub venue: Venue,
    pub score: f64,
    pub confidence: f64,
    pub horizon_ms: u64,
    pub created_at_ns: u64,
    pub expires_at_ns: u64,
    pub model_version: Option<String>,
    pub feature_set: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Signal {
    pub fn is_expired(&self, now_ns: u64) -> bool {
        now_ns >= self.expires_at_ns
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExposureEffect {
    Increase,
    ReduceOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionTarget {
    pub target_id: Uuid,
    pub strategy_id: String,
    pub asset: String,
    pub venue: Venue,
    pub target_quantity: Decimal,
    pub source_signal_id: Option<String>,
    pub created_at_ns: u64,
}

impl PositionTarget {
    pub fn asset_key(&self) -> AssetKey {
        AssetKey::new(self.venue, self.asset.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub intent_id: Uuid,
    pub strategy_id: String,
    pub asset: String,
    pub venue: Venue,
    pub side: Side,
    pub quantity: Decimal,
    pub limit_price: Option<Decimal>,
    pub effect: ExposureEffect,
    pub source_signal_id: Option<String>,
}

impl OrderIntent {
    /// Stable venue-facing identifier derived only from the persisted intent id.
    /// Replaying the same intent after a restart therefore preserves idempotency.
    pub fn client_order_id(&self) -> String {
        format!("pg{}", self.intent_id.simple())
    }

    pub fn asset_key(&self) -> AssetKey {
        AssetKey::new(self.venue, self.asset.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskDecision {
    Allow,
    Reject { code: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_order_id_is_stable_for_replay() {
        let intent = OrderIntent {
            intent_id: Uuid::parse_str("018f7f2e-6f5c-7cc4-98e8-2cd9b5c67d0f").unwrap(),
            strategy_id: "demo".into(),
            asset: "HYPE".into(),
            venue: Venue::Hyperliquid,
            side: Side::Buy,
            quantity: Decimal::ONE,
            limit_price: None,
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        };
        assert_eq!(
            intent.client_order_id(),
            "pg018f7f2e6f5c7cc498e82cd9b5c67d0f"
        );
    }
}
