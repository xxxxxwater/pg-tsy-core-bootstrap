use pg_types::{ExposureEffect, OrderIntent, Side, Signal, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct StrategyConfig {
    pub strategy_id: String,
    pub asset: String,
    pub venue: Venue,
    pub order_quantity: Decimal,
    pub entry_score: f64,
    pub exit_score: f64,
}

impl StrategyConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.order_quantity <= Decimal::ZERO {
            return Err("order_quantity must be positive");
        }
        if !(0.0..=1.0).contains(&self.entry_score) || self.entry_score == 0.0 {
            return Err("entry_score must be in (0, 1]");
        }
        if !(0.0..self.entry_score).contains(&self.exit_score) {
            return Err("exit_score must be in [0, entry_score)");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyPhase {
    Flat,
    EnteringLong,
    Long,
    ExitingLong,
    EnteringShort,
    Short,
    ExitingShort,
    SafeHold,
    Unknown,
    Halted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyState {
    pub phase: StrategyPhase,
    pub net_quantity: Decimal,
    pub active_intent_id: Option<Uuid>,
    pub last_signal_id: Option<String>,
    pub hold_reason: Option<String>,
}

impl Default for StrategyState {
    fn default() -> Self {
        Self {
            phase: StrategyPhase::Flat,
            net_quantity: Decimal::ZERO,
            active_intent_id: None,
            last_signal_id: None,
            hold_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StrategyDecision {
    Noop,
    Submit(OrderIntent),
    Hold(String),
}

pub struct StrategyMachine {
    pub config: StrategyConfig,
    pub state: StrategyState,
}

impl StrategyMachine {
    pub fn new(config: StrategyConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            config,
            state: StrategyState::default(),
        })
    }

    pub fn on_signal(&mut self, signal: &Signal, now_ns: u64) -> StrategyDecision {
        if signal.asset != self.config.asset || signal.venue != self.config.venue {
            return StrategyDecision::Noop;
        }
        if signal.is_expired(now_ns) {
            return StrategyDecision::Hold("expired signal".into());
        }
        if self.state.last_signal_id.as_deref() == Some(signal.signal_id.as_str()) {
            return StrategyDecision::Noop;
        }
        self.state.last_signal_id = Some(signal.signal_id.clone());

        match self.state.phase {
            StrategyPhase::Flat if signal.score >= self.config.entry_score => {
                self.submit_entry(signal, Side::Buy, StrategyPhase::EnteringLong)
            }
            StrategyPhase::Flat if signal.score <= -self.config.entry_score => {
                self.submit_entry(signal, Side::Sell, StrategyPhase::EnteringShort)
            }
            StrategyPhase::Long if signal.score <= self.config.exit_score => {
                self.submit_exit(signal, Side::Sell, StrategyPhase::ExitingLong)
            }
            StrategyPhase::Short if signal.score >= -self.config.exit_score => {
                self.submit_exit(signal, Side::Buy, StrategyPhase::ExitingShort)
            }
            StrategyPhase::SafeHold => StrategyDecision::Hold(
                self.state
                    .hold_reason
                    .clone()
                    .unwrap_or_else(|| "strategy is in safe hold".into()),
            ),
            StrategyPhase::Unknown => {
                StrategyDecision::Hold("state is unknown; reconcile required".into())
            }
            StrategyPhase::Halted => StrategyDecision::Hold("strategy is halted".into()),
            _ => StrategyDecision::Noop,
        }
    }

    fn submit_entry(
        &mut self,
        signal: &Signal,
        side: Side,
        next_phase: StrategyPhase,
    ) -> StrategyDecision {
        let intent = self.intent(
            signal,
            side,
            self.config.order_quantity,
            ExposureEffect::Increase,
        );
        self.state.phase = next_phase;
        self.state.active_intent_id = Some(intent.intent_id);
        StrategyDecision::Submit(intent)
    }

    fn submit_exit(
        &mut self,
        signal: &Signal,
        side: Side,
        next_phase: StrategyPhase,
    ) -> StrategyDecision {
        let quantity = self.state.net_quantity.abs();
        if quantity == Decimal::ZERO {
            self.state.phase = StrategyPhase::Unknown;
            self.state.hold_reason = Some("exit requested with zero owned quantity".into());
            return StrategyDecision::Hold("position ownership mismatch".into());
        }
        let intent = self.intent(signal, side, quantity, ExposureEffect::ReduceOnly);
        self.state.phase = next_phase;
        self.state.active_intent_id = Some(intent.intent_id);
        StrategyDecision::Submit(intent)
    }

    fn intent(
        &self,
        signal: &Signal,
        side: Side,
        quantity: Decimal,
        effect: ExposureEffect,
    ) -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: self.config.strategy_id.clone(),
            asset: self.config.asset.clone(),
            venue: self.config.venue,
            side,
            quantity,
            limit_price: None,
            effect,
            source_signal_id: Some(signal.signal_id.clone()),
        }
    }

    pub fn on_intent_filled(&mut self, intent_id: Uuid) -> bool {
        if self.state.active_intent_id != Some(intent_id) {
            return false;
        }
        match self.state.phase {
            StrategyPhase::EnteringLong => {
                self.state.net_quantity = self.config.order_quantity;
                self.state.phase = StrategyPhase::Long;
            }
            StrategyPhase::EnteringShort => {
                self.state.net_quantity = -self.config.order_quantity;
                self.state.phase = StrategyPhase::Short;
            }
            StrategyPhase::ExitingLong | StrategyPhase::ExitingShort => {
                self.state.net_quantity = Decimal::ZERO;
                self.state.phase = StrategyPhase::Flat;
            }
            _ => return false,
        }
        self.state.active_intent_id = None;
        true
    }

    pub fn on_external_state_unknown(&mut self, reason: impl Into<String>) {
        self.state.phase = StrategyPhase::Unknown;
        self.state.hold_reason = Some(reason.into());
    }

    pub fn enter_safe_hold(&mut self, reason: impl Into<String>) {
        self.state.phase = StrategyPhase::SafeHold;
        self.state.hold_reason = Some(reason.into());
        self.state.active_intent_id = None;
    }

    pub fn restore_after_reconcile(&mut self, net_quantity: Decimal) {
        self.state.net_quantity = net_quantity;
        self.state.active_intent_id = None;
        self.state.hold_reason = None;
        self.state.phase = if net_quantity > Decimal::ZERO {
            StrategyPhase::Long
        } else if net_quantity < Decimal::ZERO {
            StrategyPhase::Short
        } else {
            StrategyPhase::Flat
        };
    }

    pub fn halt(&mut self, reason: impl Into<String>) {
        self.state.phase = StrategyPhase::Halted;
        self.state.hold_reason = Some(reason.into());
        self.state.active_intent_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> StrategyConfig {
        StrategyConfig {
            strategy_id: "vwap-v4-sol".into(),
            asset: "SOLUSDT".into(),
            venue: Venue::BinancePm,
            order_quantity: Decimal::from(2),
            entry_score: 0.5,
            exit_score: 0.1,
        }
    }

    fn signal(id: &str, score: f64) -> Signal {
        Signal {
            schema_version: "signal.v1".into(),
            signal_id: id.into(),
            alpha_id: "alpha.v1".into(),
            asset: "SOLUSDT".into(),
            venue: Venue::BinancePm,
            score,
            confidence: 0.9,
            horizon_ms: 1_000,
            created_at_ns: 1,
            expires_at_ns: 2_000,
            model_version: None,
            feature_set: None,
            metadata: json!({}),
        }
    }

    #[test]
    fn long_entry_then_reduce_only_exit() {
        let mut machine = StrategyMachine::new(config()).unwrap();
        let entry = match machine.on_signal(&signal("entry", 0.8), 100) {
            StrategyDecision::Submit(intent) => intent,
            _ => panic!("expected entry intent"),
        };
        assert_eq!(entry.effect, ExposureEffect::Increase);
        assert_eq!(machine.state.phase, StrategyPhase::EnteringLong);
        assert!(machine.on_intent_filled(entry.intent_id));
        assert_eq!(machine.state.phase, StrategyPhase::Long);

        let exit = match machine.on_signal(&signal("exit", 0.0), 200) {
            StrategyDecision::Submit(intent) => intent,
            _ => panic!("expected exit intent"),
        };
        assert_eq!(exit.effect, ExposureEffect::ReduceOnly);
        assert_eq!(exit.side, Side::Sell);
        assert!(machine.on_intent_filled(exit.intent_id));
        assert_eq!(machine.state.phase, StrategyPhase::Flat);
    }

    #[test]
    fn safe_hold_blocks_new_entry() {
        let mut machine = StrategyMachine::new(config()).unwrap();
        machine.enter_safe_hold("reconcile mismatch");
        assert!(matches!(
            machine.on_signal(&signal("entry", 0.8), 100),
            StrategyDecision::Hold(_)
        ));
    }
}
