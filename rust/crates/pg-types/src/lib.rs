use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Venue {
    BinancePm,
    Hyperliquid,
    #[serde(rename = "IBKR")]
    InteractiveBrokers,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskDecision {
    Allow,
    Reject { code: String, reason: String },
}
