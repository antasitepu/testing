use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(value: impl Into<String>) -> Result<Self, SymbolError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SymbolError::Empty);
        }
        if !value
            .bytes()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        {
            return Err(SymbolError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SymbolError {
    #[error("symbol must not be empty")]
    Empty,
    #[error("symbol must contain uppercase ASCII letters and digits only")]
    InvalidCharacter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketStatus {
    Trading,
    Halted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    Regular,
    NoTrading,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Market {
    pub symbol: Symbol,
    pub enabled: bool,
    pub status: MarketStatus,
    pub price_tick: Decimal,
    pub onboard_time: i64,
    pub session_market: String,
}

impl Market {
    pub fn new(
        symbol: Symbol,
        enabled: bool,
        status: MarketStatus,
        price_tick: Decimal,
        onboard_time: i64,
        session_market: impl Into<String>,
    ) -> Result<Self, MarketError> {
        if price_tick <= Decimal::ZERO {
            return Err(MarketError::NonPositiveTick);
        }
        if onboard_time < 0 {
            return Err(MarketError::NegativeOnboardTime);
        }
        let session_market = session_market.into();
        if session_market.is_empty() {
            return Err(MarketError::EmptySessionMarket);
        }

        Ok(Self {
            symbol,
            enabled,
            status,
            price_tick,
            onboard_time,
            session_market,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MarketError {
    #[error("market price tick must be positive")]
    NonPositiveTick,
    #[error("market onboard time must not be negative")]
    NegativeOnboardTime,
    #[error("market session name must not be empty")]
    EmptySessionMarket,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MarketSession {
    pub session_market: String,
    pub status: SessionStatus,
    pub start_time: i64,
    pub end_time: i64,
    pub source_event_time: i64,
}

impl MarketSession {
    pub fn new(
        session_market: impl Into<String>,
        status: SessionStatus,
        start_time: i64,
        end_time: i64,
        source_event_time: i64,
    ) -> Result<Self, MarketSessionError> {
        let session_market = session_market.into();
        if session_market.is_empty() {
            return Err(MarketSessionError::EmptySessionMarket);
        }
        if end_time <= start_time {
            return Err(MarketSessionError::InvalidTimeRange);
        }

        Ok(Self {
            session_market,
            status,
            start_time,
            end_time,
            source_event_time,
        })
    }

    pub fn contains(&self, timestamp: i64) -> bool {
        self.start_time <= timestamp && timestamp < self.end_time
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MarketSessionError {
    #[error("market session name must not be empty")]
    EmptySessionMarket,
    #[error("market session end must be later than its start")]
    InvalidTimeRange,
}
