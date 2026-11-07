mod adx;
mod atr;
mod bollinger;
mod chandelier;
mod donchian;
mod ema;
mod rsi;

use analysis_domain::{FactId, Readiness};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub use adx::calculate_adx;
pub use atr::calculate_atr;
pub use bollinger::{BollingerFact, calculate_bollinger};
pub use chandelier::{ChandelierFact, calculate_chandelier, next_long_stop, next_short_stop};
pub use donchian::{DonchianFact, calculate_previous_donchian};
pub use ema::{EmaOutput, calculate_ema};
pub use rsi::{calculate_rsi, detect_regular_divergence};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndicatorOutput {
    pub readiness: Readiness,
    pub values: Vec<Option<Decimal>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RsiPivot {
    pub id: FactId,
    pub kind: crate::SwingKind,
    pub value: Decimal,
    pub pivot_open_time: i64,
    pub confirmed_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DivergenceKind {
    RegularBullish,
    RegularBearish,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegularDivergence {
    pub id: FactId,
    pub kind: DivergenceKind,
    pub first_price_swing_id: FactId,
    pub second_price_swing_id: FactId,
    pub first_rsi_pivot_id: FactId,
    pub second_rsi_pivot_id: FactId,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IndicatorError {
    #[error("indicator period and pivot distance must be positive")]
    InvalidPeriod,
    #[error("candles must be ordered and share one market key")]
    InvalidCandleSequence,
    #[error("indicator versions must not be blank")]
    BlankVersion,
    #[error("price and RSI pivots must share confirmed market identities")]
    InvalidPivotIdentity,
    #[error("fact ordinal exceeded u32")]
    OrdinalOverflow,
    #[error("indicator arithmetic overflowed")]
    ArithmeticOverflow,
}
