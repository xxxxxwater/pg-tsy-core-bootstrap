//! Async JEV observation boundary for the Rust strategy layer.
//!
//! An external research producer may publish a validated advisory observation
//! through a Tokio watch channel. Reading this channel is nonblocking. It can
//! veto *new exposure only*: an observation never creates a signal, quantity,
//! price, order, ownership, or permission to bypass pg-risk/OMS. All reduce-only
//! and emergency paths must stay independent of this advisory.

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
    #[error("unavailable, malformed, mismatched or stale JEV advisory")]
    InvalidObservation,
}

impl JevObservation {
    /// Bounds for a research observation, not a calibrated market probability.
    pub fn valid_at(&self, now_ns: u64) -> bool {
        if self.instrument != INSTRUMENT
            || self.model != MODEL
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
/// the Rust hot path reads without awaiting the provider's HTTP response.
pub fn channel() -> (JevAdvisoryPublisher, JevAdvisoryGate) {
    let (sender, receiver) = watch::channel(None);
    (
        JevAdvisoryPublisher { sender },
        JevAdvisoryGate { receiver },
    )
}

impl JevAdvisoryPublisher {
    /// Invalidate the previous observation on any invalid/newer response.
    pub fn publish(
        &self,
        observation: JevObservation,
        now_ns: u64,
    ) -> Result<(), JevGateError> {
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

impl JevAdvisoryGate {
    /// This method NEVER calls on_signal or creates an intent. It is a veto
    /// for an otherwise independently risk-approved signal. Reduction ignores
    /// a stale advisory, so an AI outage cannot block emergency exits.
    pub fn check(
        &self,
        signal: &Signal,
        effect: ExposureEffect,
        now_ns: u64,
    ) -> JevGateDecision {
        if effect == ExposureEffect::ReduceOnly {
            return JevGateDecision::AllowExistingDeterministicSignal;
        }
        let state = self.receiver.borrow();
        let Some(observation) = state.as_ref() else {
            return JevGateDecision::HoldNewExposure;
        };
        let signal_source = signal
            .metadata
            .get("jev_source_event_ns")
            .and_then(serde_json::Value::as_u64);
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
        assert_eq!(gate.check(&signal(), ExposureEffect::Increase, 110), JevGateDecision::HoldNewExposure);
        assert_eq!(gate.check(&signal(), ExposureEffect::ReduceOnly, 110), JevGateDecision::AllowExistingDeterministicSignal);
    }

    #[test]
    fn matching_live_observation_can_only_approve_an_existing_signal() {
        let (producer, gate) = channel();
        producer.publish(observation(), 110).unwrap();
        assert_eq!(gate.check(&signal(), ExposureEffect::Increase, 120), JevGateDecision::AllowExistingDeterministicSignal);
        assert_eq!(gate.check(&signal(), ExposureEffect::Increase, 150), JevGateDecision::HoldNewExposure);
        producer.disconnect();
        assert_eq!(gate.check(&signal(), ExposureEffect::Increase, 120), JevGateDecision::HoldNewExposure);
    }

    #[test]
    fn wrong_direction_snapshot_identity_and_invalid_response_fail_closed() {
        let (producer, gate) = channel();
        producer.publish(observation(), 110).unwrap();
        let mut wrong = signal();
        wrong.score = -0.6;
        assert_eq!(gate.check(&wrong, ExposureEffect::Increase, 120), JevGateDecision::HoldNewExposure);
        wrong = signal();
        wrong.metadata = json!({"jev_source_event_ns":99});
        assert_eq!(gate.check(&wrong, ExposureEffect::Increase, 120), JevGateDecision::HoldNewExposure);
        let mut bad = observation();
        bad.probabilities.insert("up".into(), f64::NAN);
        assert_eq!(producer.publish(bad, 111), Err(JevGateError::InvalidObservation));
        assert_eq!(gate.check(&signal(), ExposureEffect::Increase, 112), JevGateDecision::HoldNewExposure);
    }
}
