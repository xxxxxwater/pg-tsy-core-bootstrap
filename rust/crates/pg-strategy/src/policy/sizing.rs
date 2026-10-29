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
        let filled_entries = usize::try_from(context.position.filled_entries).ok()?;
        let remaining = self.remaining_position_quote(context)?;
        if filled_entries == 0 {
            // A flat position has no entry price, so both daemons report no
            // unrealized return; the first entry must not depend on it.
            return Some(self.initial_quote.min(remaining));
        }
        let current_return = context.position.unrealized_return?;
        let trigger_index = filled_entries.checked_sub(1)?;
        let trigger = *self.triggers.get(trigger_index)?;
        if current_return > -trigger {
            return None;
        }

        let scale = self.volume_scale.powi(i32::try_from(filled_entries).ok()?);
        let requested = decimal_from_f64(decimal_to_f64(self.initial_quote)? * scale)?;
        Some(requested.min(remaining))
    }

    /// Quote budget left under `max_position_quote` for the whole position,
    /// valued at cost basis (`|net_quantity| * average_entry_price`).
    ///
    /// Fails closed (`None`) when the budget is exhausted or the position
    /// cannot be valued: a non-zero quantity without an average entry price,
    /// or filled entries reported without any quantity.
    fn remaining_position_quote(&self, context: &StrategyContext<'_>) -> Option<Decimal> {
        let position = context.position;
        let current = if position.net_quantity.is_zero() {
            if position.filled_entries > 0 {
                return None;
            }
            Decimal::ZERO
        } else {
            let price = position.average_entry_price?;
            if price <= Decimal::ZERO {
                return None;
            }
            position.net_quantity.abs().checked_mul(price)?
        };
        let remaining = self.max_position_quote.checked_sub(current)?;
        (remaining > Decimal::ZERO).then_some(remaining)
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

    fn ladder() -> DcaLadderSizing {
        DcaLadderSizing {
            initial_quote: Decimal::from(1000),
            max_position_quote: Decimal::from(6000),
            triggers: vec![0.03, 0.0675, 0.105, 0.1425, 0.18],
            volume_scale: 1.1,
        }
    }

    fn quote_for(sizing: &DcaLadderSizing, position: &PositionView) -> Option<Decimal> {
        let instrument = AssetKey::new(Venue::Hyperliquid, "HYPE");
        let features = FeatureFrame::default();
        let context = StrategyContext {
            instrument: &instrument,
            features: &features,
            position,
            now_ns: 1,
        };
        sizing.next_quote_amount(&context)
    }

    fn held(quantity: i64, entry: i64, filled_entries: u32, ret: f64) -> PositionView {
        PositionView {
            net_quantity: Decimal::from(quantity),
            average_entry_price: Some(Decimal::from(entry)),
            filled_entries,
            unrealized_return: Some(ret),
            ..PositionView::default()
        }
    }

    #[test]
    fn dca_caps_by_remaining_whole_position_budget() {
        // 5 entries ~ 5600 quote already held; next step would request 1610.51.
        let quote = quote_for(&ladder(), &held(56, 100, 5, -0.20));
        assert_eq!(quote, Some(Decimal::from(400)));
    }

    #[test]
    fn dca_returns_none_when_budget_exhausted() {
        assert_eq!(quote_for(&ladder(), &held(60, 100, 4, -0.20)), None);
        assert_eq!(quote_for(&ladder(), &held(-70, 100, 4, -0.20)), None);
    }

    #[test]
    fn dca_fails_closed_on_unvalued_position() {
        let mut position = held(10, 100, 1, -0.04);
        position.average_entry_price = None;
        assert_eq!(quote_for(&ladder(), &position), None);
        let empty = PositionView {
            filled_entries: 1,
            unrealized_return: Some(-0.04),
            ..PositionView::default()
        };
        assert_eq!(quote_for(&ladder(), &empty), None);
    }

    #[test]
    fn dca_initial_entry_respects_existing_position() {
        let mut position = held(55, 100, 0, 0.0);
        assert_eq!(quote_for(&ladder(), &position), Some(Decimal::from(500)));
        position.net_quantity = Decimal::ZERO;
        position.average_entry_price = None;
        assert_eq!(quote_for(&ladder(), &position), Some(Decimal::from(1000)));
    }

    #[test]
    fn dca_first_entry_on_flat_position_needs_no_unrealized_return() {
        // What the shadow and live daemons report for a flat instrument.
        let flat = PositionView::default();
        assert_eq!(flat.unrealized_return, None);
        assert_eq!(quote_for(&ladder(), &flat), Some(Decimal::from(1000)));
        // Safety orders still require a valued return.
        let mut held = held(10, 100, 1, -0.04);
        held.unrealized_return = None;
        assert_eq!(quote_for(&ladder(), &held), None);
    }

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
            net_quantity: Decimal::from(10),
            average_entry_price: Some(Decimal::from(100)),
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
