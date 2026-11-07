use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FactKind {
    MarketClose,
    SwingHigh,
    SwingLow,
    Structure,
    Displacement,
    FairValueGap,
    Liquidity,
    LiquiditySweep,
    DealingRange,
    Rsi,
    RsiDivergence,
    Atr,
    Ema,
    Adx,
    Donchian,
    Bollinger,
    ZScore,
    Chandelier,
    SessionRange,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactId {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub kind: FactKind,
    pub confirmed_at: i64,
    pub ordinal: u32,
    pub algorithm_version: String,
    pub parameter_version: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FactLifecycle {
    Confirmed,
    PartiallyFilled,
    Filled,
    Swept,
    Invalidated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmedFact<T> {
    pub id: FactId,
    pub value: T,
    pub lifecycle: FactLifecycle,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimitiveVersion {
    pub timeframe: Timeframe,
    pub primitive: String,
    pub algorithm_version: String,
    pub parameter_version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeframeFacts {
    pub timeframe: Timeframe,
    pub evaluated_closed_candle_time: i64,
    pub facts: Vec<ConfirmedFact<serde_json::Value>>,
}

impl TimeframeFacts {
    pub fn empty(timeframe: Timeframe, evaluated_closed_candle_time: i64) -> Self {
        Self {
            timeframe,
            evaluated_closed_candle_time,
            facts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRef {
    pub fact_id: FactId,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceLevel {
    pub kind: String,
    pub price: Decimal,
}
