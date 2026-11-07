use crate::{Candle, LiveStatus, MarketSession, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClosedCandleSource {
    Live,
    CatchUp,
    HistoricalBackfill,
    DowntimeReconciled,
}

impl ClosedCandleSource {
    pub const fn work_source(self) -> Option<&'static str> {
        match self {
            Self::Live => Some("LIVE"),
            Self::CatchUp => Some("CATCH_UP"),
            Self::HistoricalBackfill => None,
            Self::DowntimeReconciled => Some("DOWNTIME_RECONCILED"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClosedCandleWrite {
    pub candle: Candle,
    pub source: ClosedCandleSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AggregateTrade {
    pub symbol: Symbol,
    pub aggregate_trade_id: i64,
    pub first_trade_id: i64,
    pub last_trade_id: i64,
    pub event_time: i64,
    pub trade_time: i64,
    pub price: Decimal,
}

impl AggregateTrade {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        symbol: Symbol,
        aggregate_trade_id: i64,
        first_trade_id: i64,
        last_trade_id: i64,
        event_time: i64,
        trade_time: i64,
        price: Decimal,
    ) -> Result<Self, AggregateTradeError> {
        if aggregate_trade_id < 0 || first_trade_id < 0 || last_trade_id < first_trade_id {
            return Err(AggregateTradeError::InvalidTradeIdRange);
        }
        if event_time < 0 || trade_time < 0 {
            return Err(AggregateTradeError::NegativeTime);
        }
        if price <= Decimal::ZERO {
            return Err(AggregateTradeError::NonPositivePrice);
        }

        Ok(Self {
            symbol,
            aggregate_trade_id,
            first_trade_id,
            last_trade_id,
            event_time,
            trade_time,
            price,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AggregateTradeError {
    #[error("aggregate trade identity range is invalid")]
    InvalidTradeIdRange,
    #[error("aggregate trade time must not be negative")]
    NegativeTime,
    #[error("aggregate trade price must be positive")]
    NonPositivePrice,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MarketEvent {
    FormingCandle(Candle),
    ClosedCandle(Candle),
    AggregateTrade(AggregateTrade),
    SessionChanged(MarketSession),
    StatusChanged { symbol: Symbol, status: LiveStatus },
    ResyncRequired { symbol: Symbol, reason: String },
}
