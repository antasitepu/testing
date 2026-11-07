use crate::{PrimitiveVersion, Readiness, TimeframeFacts};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalMarketSnapshot {
    pub symbol: Symbol,
    pub target_timeframe: Timeframe,
    pub price_tick: Decimal,
    pub evaluated_closed_candle_time: i64,
    pub readiness: Readiness,
    pub target: TimeframeFacts,
    pub higher_timeframes: Vec<TimeframeFacts>,
    pub primitive_versions: Vec<PrimitiveVersion>,
}

impl CanonicalMarketSnapshot {
    pub fn empty(
        symbol: Symbol,
        target_timeframe: Timeframe,
        price_tick: Decimal,
        evaluated_closed_candle_time: i64,
        readiness: Readiness,
    ) -> Result<Self, SnapshotError> {
        if evaluated_closed_candle_time < 0 {
            return Err(SnapshotError::NegativeEvaluationTime);
        }
        if price_tick <= Decimal::ZERO {
            return Err(SnapshotError::NonPositivePriceTick);
        }
        Ok(Self {
            symbol,
            target_timeframe,
            price_tick,
            evaluated_closed_candle_time,
            readiness,
            target: TimeframeFacts::empty(target_timeframe, evaluated_closed_candle_time),
            higher_timeframes: Vec::new(),
            primitive_versions: Vec::new(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SnapshotError {
    #[error("evaluated closed candle time must not be negative")]
    NegativeEvaluationTime,
    #[error("price tick must be positive")]
    NonPositivePriceTick,
}
