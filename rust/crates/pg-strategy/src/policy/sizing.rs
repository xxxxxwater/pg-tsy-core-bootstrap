use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::StrategyContext;

pub trait SizingPolicy: Send + Sync {
    fn quantity(&self, context: &StrategyContext<'_>, reference_price: Decimal) -> Option<Decimal>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedQuantitySizing {
    pub quantity: Decimal,
}

impl SizingPolicy for FixedQuantitySizing {
    fn quantity(
        &self,
        _context: &StrategyContext<'_>,
        _reference_price: Decimal,
    ) -> Option<Decimal> {
        (self.quantity > Decimal::ZERO).then_some(self.quantity)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcaLadderSizing {
    /// Quote-currency amount for the first entry.
    pub initial_quote: Decimal,
    /// Hard cap for the whole owned position in quote currency.
    pub max_position_quote: Decimal,
    /// Loss thresholds expressed as positive fractions, e.g. 0.03 for -3%.
    pub triggers: Vec<f64>,
    /// Multiplier for each successive safety order.
    pub volume_scale: f64,
}

impl DcaLadderSizing {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.initial_quote <= Decimal::ZERO || self.max_position_quote <= Decimal::ZERO {
            return Err("DCA quote budgets must be positive");
        }
        if self.max_position_quote < self.initial_quote {
            return Err("DCA max_position_quote must cover initial_quote");
        }
        if self
            .triggers
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err("DCA triggers must be finite positive loss fractions");
        }
        if !self.volume_scale.is_finite() || self.volume_scale <= 0.0 {
            return Err("DCA volume_scale must be finite and positive");
        }
        Ok(())
    }

    pub fn next_quote_amount(&self, context: &StrategyContext<'_>) -> Option<Decimal> {
        self.validate().ok()?;
        let current_return = context.position.unrealized_return?;
        let filled_entries = usize::try_from(context.position.filled_entries).ok()?;
        if filled_entries == 0 {
            return Some(self.initial_quote.min(self.max_position_quote));
        }
        let trigger_index = filled_entries.checked_sub(1)?;
        let trigger = *self.triggers.get(trigger_index)?;
        if current_return > -trigger {
            return None;
        }

        let scale = self.volume_scale.powi(i32::try_from(filled_entries).ok()?);
        let requested = decimal_from_f64(decimal_to_f64(self.initial_quote)? * scale)?;
        Some(requested.min(self.max_position_quote))
    }
}

fn decimal_to_f64(value: Decimal) -> Option<f64> {
    value.to_string().parse().ok()
}

fn decimal_from_f64(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    value.to_string().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{FeatureFrame, PositionView, StrategyContext};
    use pg_types::{AssetKey, Venue};

    #[test]
    fn dca_trigger_is_venue_neutral() {
        let sizing = DcaLadderSizing {
            initial_quote: Decimal::from(1000),
            max_position_quote: Decimal::from(6000),
            triggers: vec![0.03, 0.0675, 0.105, 0.1425, 0.18],
            volume_scale: 1.1,
        };
        let features = FeatureFrame::default();
        let position = PositionView {
            filled_entries: 1,
            unrealized_return: Some(-0.04),
            ..PositionView::default()
        };
        for instrument in [
            AssetKey::new(Venue::BinancePm, "ETHUSDT"),
            AssetKey::new(Venue::Hyperliquid, "HYPE"),
            AssetKey::new(Venue::InteractiveBrokers, "AAPL"),
        ] {
            let context = StrategyContext {
                instrument: &instrument,
                features: &features,
                position: &position,
                now_ns: 1,
            };
            assert_eq!(
                sizing.next_quote_amount(&context),
                Some(Decimal::from(1100))
            );
        }
    }
}
