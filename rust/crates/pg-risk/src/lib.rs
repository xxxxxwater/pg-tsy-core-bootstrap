use pg_types::{ExposureEffect, OrderIntent, RiskDecision, Signal};

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub min_confidence: f64,
    pub max_abs_signal_score: f64,
    pub allow_new_exposure: bool,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self { min_confidence: 0.50, max_abs_signal_score: 1.0, allow_new_exposure: false }
    }
}

pub fn evaluate_signal(signal: &Signal, now_ns: u64, limits: &RiskLimits) -> RiskDecision {
    if signal.is_expired(now_ns) {
        return RiskDecision::Reject { code: "SIGNAL_EXPIRED".into(), reason: "signal TTL elapsed".into() };
    }
    if !(0.0..=1.0).contains(&signal.confidence) || signal.confidence < limits.min_confidence {
        return RiskDecision::Reject { code: "LOW_CONFIDENCE".into(), reason: "confidence below limit".into() };
    }
    if signal.score.abs() > limits.max_abs_signal_score {
        return RiskDecision::Reject { code: "INVALID_SCORE".into(), reason: "score outside configured bound".into() };
    }
    RiskDecision::Allow
}

pub fn evaluate_order(intent: &OrderIntent, limits: &RiskLimits) -> RiskDecision {
    if intent.quantity <= rust_decimal::Decimal::ZERO {
        return RiskDecision::Reject { code: "INVALID_QTY".into(), reason: "quantity must be positive".into() };
    }
    if intent.effect == ExposureEffect::Increase && !limits.allow_new_exposure {
        return RiskDecision::Reject { code: "NEW_EXPOSURE_DISABLED".into(), reason: "live/new exposure gate is closed".into() };
    }
    RiskDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_types::Venue;
    use serde_json::json;

    #[test]
    fn expired_signal_fails_closed() {
        let s = Signal { schema_version:"signal.v1".into(), signal_id:"s".into(), alpha_id:"a".into(), asset:"SOLUSDT".into(), venue:Venue::BinancePm, score:0.2, confidence:0.8, horizon_ms:10, created_at_ns:1, expires_at_ns:10, model_version:None, feature_set:None, metadata:json!({}) };
        assert!(matches!(evaluate_signal(&s, 10, &RiskLimits::default()), RiskDecision::Reject{..}));
    }
}
