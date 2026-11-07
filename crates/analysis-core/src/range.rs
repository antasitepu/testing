use crate::{ConfirmedSwing, SwingKind};
use analysis_domain::{FactId, FactKind};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RangeRegion {
    Discount,
    Equilibrium,
    Premium,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DealingRange {
    pub id: FactId,
    pub low_swing_id: FactId,
    pub high_swing_id: FactId,
    pub low: Decimal,
    pub high: Decimal,
    pub equilibrium: Decimal,
}

impl DealingRange {
    pub fn region(&self, price: Decimal) -> RangeRegion {
        if price < self.equilibrium {
            RangeRegion::Discount
        } else if price > self.equilibrium {
            RangeRegion::Premium
        } else {
            RangeRegion::Equilibrium
        }
    }
}

pub fn build_dealing_range(
    low: &ConfirmedSwing,
    high: &ConfirmedSwing,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<DealingRange, RangeError> {
    if low.kind != SwingKind::Low
        || high.kind != SwingKind::High
        || low.id.symbol != high.id.symbol
        || low.id.timeframe != high.id.timeframe
        || low.price >= high.price
        || algorithm_version.trim().is_empty()
        || parameter_version.trim().is_empty()
    {
        return Err(RangeError::InvalidBounds);
    }
    Ok(DealingRange {
        id: FactId {
            symbol: low.id.symbol.clone(),
            timeframe: low.id.timeframe,
            kind: FactKind::DealingRange,
            confirmed_at: low.confirmed_at.max(high.confirmed_at),
            ordinal: 0,
            algorithm_version: algorithm_version.to_owned(),
            parameter_version: parameter_version.to_owned(),
        },
        low_swing_id: low.id.clone(),
        high_swing_id: high.id.clone(),
        low: low.price,
        high: high.price,
        equilibrium: (low.price + high.price) / Decimal::TWO,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RangeError {
    #[error("dealing range requires matching low/high swings with ordered prices")]
    InvalidBounds,
}
