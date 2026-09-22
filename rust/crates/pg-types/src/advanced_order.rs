use crate::{OrderIntent, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    #[default]
    Gtc,
    Ioc,
    Fok,
    Gtd { expires_at_ns: u64 },
    Day,
    AtTheOpen,
    AtTheClose,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OrderConstraints {
    #[serde(default)]
    pub post_only: bool,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub iceberg_display_quantity: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompositeInstruction {
    #[default]
    Single,
    Oco { group_id: String },
    Ouo { group_id: String },
    Oto { parent_client_order_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedOrderIntent {
    pub base: OrderIntent,
    #[serde(default)]
    pub time_in_force: TimeInForce,
    #[serde(default)]
    pub constraints: OrderConstraints,
    #[serde(default)]
    pub composite: CompositeInstruction,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdvancedOrderError {
    NonPositiveQuantity,
    PostOnlyMarket,
    InvalidIceberg,
    InvalidGtd,
    EmptyCompositeId,
    ReduceOnlyMismatch,
}

impl AdvancedOrderIntent {
    pub fn validate(&self) -> Result<(), AdvancedOrderError> {
        if self.base.quantity <= Decimal::ZERO {
            return Err(AdvancedOrderError::NonPositiveQuantity);
        }
        if self.constraints.post_only && self.base.limit_price.is_none() {
            return Err(AdvancedOrderError::PostOnlyMarket);
        }
        if let Some(display) = self.constraints.iceberg_display_quantity
            && (display <= Decimal::ZERO || display >= self.base.quantity)
        {
            return Err(AdvancedOrderError::InvalidIceberg);
        }
        if matches!(self.time_in_force, TimeInForce::Gtd { expires_at_ns: 0 }) {
            return Err(AdvancedOrderError::InvalidGtd);
        }
        match &self.composite {
            CompositeInstruction::Oco { group_id } | CompositeInstruction::Ouo { group_id }
                if group_id.trim().is_empty() =>
            {
                return Err(AdvancedOrderError::EmptyCompositeId);
            }
            CompositeInstruction::Oto {
                parent_client_order_id,
            } if parent_client_order_id.trim().is_empty() => {
                return Err(AdvancedOrderError::EmptyCompositeId);
            }
            _ => {}
        }
        let base_reduce_only = matches!(self.base.effect, crate::ExposureEffect::ReduceOnly);
        if self.constraints.reduce_only != base_reduce_only {
            return Err(AdvancedOrderError::ReduceOnlyMismatch);
        }
        Ok(())
    }

    pub fn side(&self) -> Side {
        self.base.side
    }

    pub fn visible_quantity(&self) -> Decimal {
        self.constraints
            .iceberg_display_quantity
            .unwrap_or(self.base.quantity)
            .min(self.base.quantity)
    }
}

impl std::fmt::Display for AdvancedOrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NonPositiveQuantity => "quantity must be positive",
            Self::PostOnlyMarket => "post-only requires a limit price",
            Self::InvalidIceberg => {
                "iceberg display quantity must be positive and smaller than total quantity"
            }
            Self::InvalidGtd => "GTD expiry must be non-zero",
            Self::EmptyCompositeId => "composite group/id must not be empty",
            Self::ReduceOnlyMismatch => "reduce-only constraint disagrees with base order effect",
        };
        f.write_str(message)
    }
}

impl std::error::Error for AdvancedOrderError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExposureEffect, Venue};
    use uuid::Uuid;

    fn base() -> OrderIntent {
        OrderIntent {
            intent_id: Uuid::new_v4(),
            strategy_id: "test".into(),
            asset: "BTCUSDC".into(),
            venue: Venue::BinancePm,
            side: Side::Buy,
            quantity: Decimal::from(10),
            limit_price: Some(Decimal::from(100)),
            effect: ExposureEffect::Increase,
            source_signal_id: None,
        }
    }

    #[test]
    fn validates_iceberg_and_post_only() {
        let order = AdvancedOrderIntent {
            base: base(),
            time_in_force: TimeInForce::Gtc,
            constraints: OrderConstraints {
                post_only: true,
                reduce_only: false,
                iceberg_display_quantity: Some(Decimal::from(2)),
            },
            composite: CompositeInstruction::Single,
        };
        assert_eq!(order.visible_quantity(), Decimal::from(2));
        order.validate().unwrap();
    }

    #[test]
    fn reduce_only_must_match_base_effect() {
        let mut order = AdvancedOrderIntent {
            base: base(),
            time_in_force: TimeInForce::Gtc,
            constraints: OrderConstraints {
                reduce_only: true,
                ..OrderConstraints::default()
            },
            composite: CompositeInstruction::Single,
        };
        assert_eq!(
            order.validate(),
            Err(AdvancedOrderError::ReduceOnlyMismatch)
        );
        order.base.effect = ExposureEffect::ReduceOnly;
        order.validate().unwrap();
    }
}
