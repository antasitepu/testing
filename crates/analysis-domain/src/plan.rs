use crate::{VersionIdentity, VersionIdentityError};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanKind {
    Ict,
    Smc,
    Rsi,
    Quasimodo,
    TrendBreakout,
    MeanReversion,
}

impl PlanKind {
    pub const ALL: [Self; 6] = [
        Self::Ict,
        Self::Smc,
        Self::Rsi,
        Self::Quasimodo,
        Self::TrendBreakout,
        Self::MeanReversion,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ict => "ICT",
            Self::Smc => "SMC",
            Self::Rsi => "RSI",
            Self::Quasimodo => "QUASIMODO",
            Self::TrendBreakout => "TREND_BREAKOUT",
            Self::MeanReversion => "MEAN_REVERSION",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Buy,
    Sell,
    None,
}

impl Direction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
            Self::None => "NONE",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessReason {
    InsufficientHistory,
    MissingHigherTimeframe,
    UnresolvedGap,
    InvalidConfiguration,
}

impl ReadinessReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InsufficientHistory => "insufficient_history",
            Self::MissingHigherTimeframe => "missing_higher_timeframe",
            Self::UnresolvedGap => "unresolved_gap",
            Self::InvalidConfiguration => "invalid_configuration",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Readiness {
    NotReady { reason: ReadinessReason },
    Ready,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SetupState {
    Neutral,
    Watching,
    EntryAvailable,
    Invalidated,
    Expired,
}

impl SetupState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "NEUTRAL",
            Self::Watching => "WATCHING",
            Self::EntryAvailable => "ENTRY_AVAILABLE",
            Self::Invalidated => "INVALIDATED",
            Self::Expired => "EXPIRED",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeState {
    NotEntered,
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrackingHealth {
    Live,
    Stale,
    Reconciling,
    MarketClosed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionInstruction {
    LimitZone,
    NextOpen,
}

impl ExecutionInstruction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LimitZone => "LIMIT_ZONE",
            Self::NextOpen => "NEXT_OPEN",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPlanProjection {
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub target_timeframe: Timeframe,
    pub readiness: Readiness,
    pub setup_state: SetupState,
    pub direction: Direction,
    pub trade_state: TradeState,
    pub tracking_health: TrackingHealth,
    pub active_signal_id: Option<Uuid>,
    pub active_virtual_trade_id: Option<Uuid>,
    pub version: VersionIdentity,
    pub evaluated_closed_candle_time: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanFeedback {
    ActualEntry {
        signal_id: Uuid,
        virtual_trade_id: Uuid,
        time: i64,
        price: Decimal,
    },
    PartialTargetFilled {
        signal_id: Uuid,
        virtual_trade_id: Uuid,
        time: i64,
        price: Decimal,
        realized_fraction: Decimal,
        remaining_fraction: Decimal,
    },
    TradeClosed {
        signal_id: Uuid,
        virtual_trade_id: Uuid,
        time: i64,
        reason: String,
    },
}

impl From<VersionIdentityError> for ReadinessReason {
    fn from(_: VersionIdentityError) -> Self {
        Self::InvalidConfiguration
    }
}
