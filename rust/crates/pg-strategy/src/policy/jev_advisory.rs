//! Async System-One observation boundary for the Rust strategy layer.
//!
//! Jev remains the primary provider. A pinned Laya deployment may provide a
//! Jev-compatible fallback observation after Jev is unavailable. The producer
//! publishes only a validated immutable advisory through a Tokio watch channel.
//! Reading this channel is nonblocking. An observation can veto *new exposure
//! only*: it never creates a signal, quantity, price, order, ownership, or
//! permission to bypass pg-risk/OMS. All reduce-only and emergency paths stay
//! independent of model availability.

use std::collections::BTreeMap;

use pg_types::{ExposureEffect, Signal, Venue};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

pub const MODEL: &str = "jev-1.13.0";
pub const INSTRUMENT: &str = "BINANCE_PM:BTCUSDC";
const MAX_TTL_NS: u64 = 5_000_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevObservation {
    pub instrument: String,
    /// Primary Jev uses the exact pinned MODEL above.
    /// Laya fallback uses:
    /// laya@<40-char git sha>:<runtime model>[:<routing model>]
    pub model: String,
    pub source_event_ns: u64,
    pub received_ns: u64,
    pub expires_ns: u64,
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    pub provider_confidence: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JevGateError {
    #[error("unavailable, malformed, mismatched or stale System-One advisory")]
    InvalidObservation,
}

fn is_pinned_laya_model(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("laya@") else {
        return false;
    };
    let Some((build, runtime)) = rest.split_once(':') else {
        return false;
    };
    build.len() == 40
        && build
            .bytes()
            .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))
        && !runtime.is_empty()
        && runtime.len() <= 160
        && !runtime.chars().any(char::is_whitespace)
}

fn is_supported_model(model: &str) -> bool {
    model == MODEL || is_pinned_laya_model(model)
}

impl JevObservation {
    /// Bounds for a research observation, not a calibrated market probability.
    pub fn valid_at(&self, now_ns: u64) -> bool {
        if self.instrument != INSTRUMENT
            || !is_supported_model(&self.model)
            || self.source_event_ns == 0
            || self.source_event_ns > self.received_ns
            || self.received_ns > now_ns
            || now_ns >= self.expires_ns
            || self.expires_ns <= self.source_event_ns
            || self.expires_ns - self.source_event_ns > MAX_TTL_NS
            || !matches!(self.choice.as_str(), "up" | "down" | "neutral")
            || !self.provider_confidence.is_finite()
            || !(0.0..=1.0).contains(&self.provider_confidence)
            || self.probabilities.len() != 3
        {
            return false;
        }
        let mut total = 0.0;
        let mut highest = 0.0_f64;
        for key in ["up", "down", "neutral"] {
            let Some(probability) = self.probabilities.get(key).copied() else {
                return false;
            };
            if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
                return false;
            }
            total += probability;
            highest = highest.max(probability);
        }
        (total - 1.0).abs() <= 0.001
            && self
                .probabilities
                .get(&self.choice)
                .is_some_and(|winner| *winner >= highest - 0.001)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevGateDecision {
    AllowExistingDeterministicSignal,
    HoldNewExposure,
}

pub struct JevAdvisoryPublisher {
    sender: watch::Sender<Option<JevObservation>>,
}

pub struct JevAdvisoryGate {
    receiver: watch::Receiver<Option<JevObservation>>,
}

/// An asynchronous producer sends the most recent immutable observation;
/// the Rust hot path reads without awaiting either provider's HTTP response.
pub fn channel() -> (JevAdvisoryPublisher, JevAdvisoryGate) {
    let (sender, receiver) = watch::channel(None);
    (
        JevAdvisoryPublisher { sender },
        JevAdvisoryGate { receiver },
    )
}

impl JevAdvisoryPublisher {
    /// Invalidate the previous observation on any invalid/newer response.
    pub fn publish(&self, observation: JevObservation, now_ns: u64) -> Result<(), JevGateError> {
        if !observation.valid_at(now_ns) {
            self.sender.send_replace(None);
            return Err(JevGateError::InvalidObservation);
        }
        self.sender.send_replace(Some(observation));
        Ok(())
    }

    pub fn disconnect(&self) {
        self.sender.send_replace(None);
    }
}

fn signal_source_event_ns(signal: &Signal) -> Option<u64> {
    let generic = signal
        .metadata
        .get("system_one_source_event_ns")
        .and_then(serde_json::Value::as_u64);
    let legacy = signal
        .metadata
        .get("jev_source_event_ns")
        .and_then(serde_json::Value::as_u64);
    match (generic, legacy) {
        (Some(generic), Some(legacy)) if generic != legacy => None,
        (Some(generic), _) => Some(generic),
        (_, Some(legacy)) => Some(legacy),
        _ => None,
    }
}

impl JevAdvisoryGate {
    /// This method NEVER calls on_signal or creates an intent. It is a veto
    /// for an otherwise independently risk-approved signal. Reduction ignores
    /// a stale advisory, so a provider outage cannot block emergency exits.
    pub fn check(&self, signal: &Signal, effect: ExposureEffect, now_ns: u64) -> JevGateDecision {
        if effect == ExposureEffect::ReduceOnly {
            return JevGateDecision::AllowExistingDeterministicSignal;
        }
        let state = self.receiver.borrow();
        let Some(observation) = state.as_ref() else {
            return JevGateDecision::HoldNewExposure;
        };
        let signal_source = signal_source_event_ns(signal);
        if signal.venue != Venue::BinancePm
            || signal.asset != "BTCUSDC"
            || signal.is_expired(now_ns)
            || signal.created_at_ns < observation.source_event_ns
            || signal_source != Some(observation.source_event_ns)
            || !observation.valid_at(now_ns)
        {
            return JevGateDecision::HoldNewExposure;
        }
        let agrees = match observation.choice.as_str() {
            "up" => signal.score > 0.0,
            "down" => signal.score < 0.0,
            _ => false,
        };
        if agrees {
            JevGateDecision::AllowExistingDeterministicSignal
        } else {
            JevGateDecision::HoldNewExposure
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LAYA_BUILD: &str = "ec8409e542941bb4bb649d5fec00d4cec96ae024";

    fn observation() -> JevObservation {
        JevObservation {
            instrument: INSTRUMENT.into(),
            model: MODEL.into(),
            source_event_ns: 100,
            received_ns: 105,
            expires_ns: 150,
            choice: "up".into(),
            probabilities: BTreeMap::from([
                ("up".into(), 0.7),
                ("down".into(), 0.2),
                ("neutral".into(), 0.1),
            ]),
            provider_confidence: 0.8,
            input_tokens: 10,
            output_tokens: 2,
        }
    }

    fn laya_observation() -> JevObservation {
        JevObservation {
            model: format!("laya@{LAYA_BUILD}:laya-rl-agent:typed-decisions"),
            ..observation()
        }
    }

    fn signal() -> Signal {
        Signal {
            schema_version: "signal.v1".into(),
            signal_id: "deterministic-signal".into(),
            alpha_id: "alpha.v1".into(),
            asset: "BTCUSDC".into(),
            venue: Venue::BinancePm,
            score: 0.6,
            confidence: 0.9,
            horizon_ms: 1_000,
            created_at_ns: 106,
            expires_at_ns: 160,
            model_version: None,
            feature_set: None,
            metadata: json!({"jev_source_event_ns":100}),
        }
    }

    #[test]
    fn no_observation_blocks_new_exposure_but_not_reduction() {
        let (_, gate) = channel();
        assert_eq!(
            gate.check(&signal(), ExposureEffect::Increase, 110),
            JevGateDecision::HoldNewExposure
        );
        assert_eq!(
            gate.check(&signal(), ExposureEffect::ReduceOnly, 110),
            JevGateDecision::AllowExistingDeterministicSignal
        );
    }

    #[test]
    fn matching_live_observation_can_only_approve_an_existing_signal() {
        let (producer, gate) = channel();
        producer.publish(observation(), 110).unwrap();
        assert_eq!(
            gate.check(&signal(), ExposureEffect::Increase, 120),
            JevGateDecision::AllowExistingDeterministicSignal
        );
        assert_eq!(
            gate.check(&signal(), ExposureEffect::Increase, 150),
            JevGateDecision::HoldNewExposure
        );
        producer.disconnect();
        assert_eq!(
            gate.check(&signal(), ExposureEffect::Increase, 120),
            JevGateDecision::HoldNewExposure
        );
    }

    #[test]
    fn pinned_laya_fallback_uses_same_advisory_veto_contract() {
        let (producer, gate) = channel();
        producer.publish(laya_observation(), 110).unwrap();
        let mut generic = signal();
        generic.metadata = json!({"system_one_source_event_ns":100});
        assert_eq!(
            gate.check(&generic, ExposureEffect::Increase, 120),
            JevGateDecision::AllowExistingDeterministicSignal
        );

        let mut unpinned = laya_observation();
        unpinned.model = "laya@unpinned:laya-rl-agent:typed-decisions".into();
        assert_eq!(
            producer.publish(unpinned, 111),
            Err(JevGateError::InvalidObservation)
        );
        assert_eq!(
            gate.check(&generic, ExposureEffect::Increase, 112),
            JevGateDecision::HoldNewExposure
        );
    }

    #[test]
    fn conflicting_generic_and_legacy_snapshot_ids_fail_closed() {
        let (producer, gate) = channel();
        producer.publish(observation(), 110).unwrap();
        let mut conflicting = signal();
        conflicting.metadata =
            json!({"system_one_source_event_ns":100, "jev_source_event_ns":99});
        assert_eq!(
            gate.check(&conflicting, ExposureEffect::Increase, 120),
            JevGateDecision::HoldNewExposure
        );
    }

    #[test]
    fn wrong_direction_snapshot_identity_and_invalid_response_fail_closed() {
        let (producer, gate) = channel();
        producer.publish(observation(), 110).unwrap();
        let mut wrong = signal();
        wrong.score = -0.6;
        assert_eq!(
            gate.check(&wrong, ExposureEffect::Increase, 120),
            JevGateDecision::HoldNewExposure
        );
        wrong = signal();
        wrong.metadata = json!({"jev_source_event_ns":99});
        assert_eq!(
            gate.check(&wrong, ExposureEffect::Increase, 120),
            JevGateDecision::HoldNewExposure
        );
        let mut bad = observation();
        bad.probabilities.insert("up".into(), f64::NAN);
        assert_eq!(
            producer.publish(bad, 111),
            Err(JevGateError::InvalidObservation)
        );
        assert_eq!(
            gate.check(&signal(), ExposureEffect::Increase, 112),
            JevGateDecision::HoldNewExposure
        );
    }
}
