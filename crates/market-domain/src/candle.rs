use crate::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Candle {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub open_time: i64,
    pub close_time: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Option<Decimal>,
    pub taker_buy_volume: Option<Decimal>,
}

impl Candle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        symbol: Symbol,
        timeframe: Timeframe,
        open_time: i64,
        close_time: i64,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
    ) -> Result<Self, CandleError> {
        if close_time < open_time {
            return Err(CandleError::InvalidTimeRange);
        }
        if [open, high, low, close]
            .into_iter()
            .any(|price| price <= Decimal::ZERO)
        {
            return Err(CandleError::NonPositivePrice);
        }
        if high < open.max(close) {
            return Err(CandleError::HighBelowBody);
        }
        if low > open.min(close) {
            return Err(CandleError::LowAboveBody);
        }
        if low > high {
            return Err(CandleError::LowAboveHigh);
        }

        Ok(Self {
            symbol,
            timeframe,
            open_time,
            close_time,
            open,
            high,
            low,
            close,
            volume: None,
            taker_buy_volume: None,
        })
    }

    pub fn with_volume(
        mut self,
        volume: Decimal,
        taker_buy_volume: Decimal,
    ) -> Result<Self, CandleError> {
        if volume < Decimal::ZERO || taker_buy_volume < Decimal::ZERO {
            return Err(CandleError::NegativeVolume);
        }
        if taker_buy_volume > volume {
            return Err(CandleError::TakerBuyVolumeAboveVolume);
        }
        self.volume = Some(volume);
        self.taker_buy_volume = Some(taker_buy_volume);
        Ok(self)
    }

    pub fn is_closed_at(&self, timestamp: i64) -> bool {
        self.close_time <= timestamp
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CandleError {
    #[error("candle close time is earlier than open time")]
    InvalidTimeRange,
    #[error("candle prices must be positive")]
    NonPositivePrice,
    #[error("candle high is below the open or close")]
    HighBelowBody,
    #[error("candle low is above the open or close")]
    LowAboveBody,
    #[error("candle low is above candle high")]
    LowAboveHigh,
    #[error("candle volume must not be negative")]
    NegativeVolume,
    #[error("candle taker buy volume must not exceed total volume")]
    TakerBuyVolumeAboveVolume,
}
