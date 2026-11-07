use crate::{Direction, EvidenceRef, ExecutionInstruction, PlanKind, ReferenceLevel};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceZone {
    pub low: Decimal,
    pub high: Decimal,
}

impl PriceZone {
    pub fn new(low: Decimal, high: Decimal) -> Result<Self, PriceZoneError> {
        if low <= Decimal::ZERO || high <= Decimal::ZERO {
            return Err(PriceZoneError::NonPositive);
        }
        if low > high {
            return Err(PriceZoneError::Inverted);
        }
        Ok(Self { low, high })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PriceZoneError {
    #[error("price zone bounds must be positive")]
    NonPositive,
    #[error("price zone low must not exceed high")]
    Inverted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetLeg {
    pub trigger_r: Decimal,
    pub fraction: Decimal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalExecutionPlan {
    pub thesis_invalidation: Decimal,
    pub hard_stop: Decimal,
    pub tp1: TargetLeg,
    pub tp2_zone: PriceZone,
}

impl SignalExecutionPlan {
    pub fn validate(
        &self,
        direction: Direction,
        entry_reference: Decimal,
        analytical_target: Decimal,
    ) -> Result<(), SignalExecutionPlanError> {
        if [
            self.thesis_invalidation,
            self.hard_stop,
            self.tp1.trigger_r,
            self.tp1.fraction,
            self.tp2_zone.low,
            self.tp2_zone.high,
            entry_reference,
            analytical_target,
        ]
        .iter()
        .any(|value| *value <= Decimal::ZERO)
        {
            return Err(SignalExecutionPlanError::NonPositiveValue);
        }
        if self.tp1.fraction != Decimal::new(5, 1) {
            return Err(SignalExecutionPlanError::InvalidFraction);
        }
        if self.tp2_zone.low > self.tp2_zone.high {
            return Err(SignalExecutionPlanError::InvalidTargetZone);
        }

        match direction {
            Direction::Buy => {
                let tp2_edge = self.tp2_zone.low;
                if self.hard_stop >= self.thesis_invalidation
                    || self.thesis_invalidation > entry_reference
                    || entry_reference >= tp2_edge
                    || analytical_target <= tp2_edge
                    || analytical_target > self.tp2_zone.high
                {
                    return Err(SignalExecutionPlanError::InvalidGeometry);
                }
                let tp1_price =
                    entry_reference + (entry_reference - self.hard_stop) * self.tp1.trigger_r;
                if tp1_price > tp2_edge {
                    return Err(SignalExecutionPlanError::Tp1BeyondTarget);
                }
            }
            Direction::Sell => {
                let tp2_edge = self.tp2_zone.high;
                if tp2_edge >= entry_reference
                    || entry_reference > self.thesis_invalidation
                    || self.thesis_invalidation >= self.hard_stop
                    || analytical_target < self.tp2_zone.low
                    || analytical_target >= tp2_edge
                {
                    return Err(SignalExecutionPlanError::InvalidGeometry);
                }
                let tp1_price =
                    entry_reference - (self.hard_stop - entry_reference) * self.tp1.trigger_r;
                if tp1_price < tp2_edge {
                    return Err(SignalExecutionPlanError::Tp1BeyondTarget);
                }
            }
            Direction::None => return Err(SignalExecutionPlanError::MissingDirection),
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SignalExecutionPlanError {
    #[error("execution plan values must be positive")]
    NonPositiveValue,
    #[error("TP1 fraction must equal 0.5")]
    InvalidFraction,
    #[error("TP2 zone low must not exceed high")]
    InvalidTargetZone,
    #[error("execution plan requires a BUY or SELL direction")]
    MissingDirection,
    #[error("signal has no projected entry or entry zone")]
    MissingEntryReference,
    #[error("execution plan geometry is invalid for the signal direction")]
    InvalidGeometry,
    #[error("candidate TP1 is beyond the TP2 execution edge")]
    Tp1BeyondTarget,
    #[error("execution plan hard stop must equal signal stop loss")]
    HardStopMismatch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreEntryValidation {
    pub minimum_rr: Decimal,
    pub reject_if_target_crossed: bool,
    pub reject_if_stop_crossed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HtfAlignment {
    Aligned,
    Counter,
    Neutral,
    Missing,
    Off,
}

impl HtfAlignment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aligned => "ALIGNED",
            Self::Counter => "COUNTER",
            Self::Neutral => "NEUTRAL",
            Self::Missing => "MISSING",
            Self::Off => "OFF",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HtfContext {
    pub higher_timeframe: Option<Timeframe>,
    pub direction: Direction,
    pub alignment: HtfAlignment,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalDefinition {
    pub signal_id: Uuid,
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub target_timeframe: Timeframe,
    pub evaluated_at: i64,
    pub direction: Direction,
    pub execution_instruction: ExecutionInstruction,
    pub entry_zone: Option<PriceZone>,
    pub projected_entry: Option<Decimal>,
    pub stop_loss: Decimal,
    pub primary_target: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_plan: Option<SignalExecutionPlan>,
    pub reference_levels: Vec<ReferenceLevel>,
    pub risk_reward: Decimal,
    pub pre_entry_validation: Option<PreEntryValidation>,
    pub confidence: Decimal,
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub htf_context: Option<HtfContext>,
    pub effective_at: i64,
    pub detected_at: i64,
    pub published_at: i64,
    pub release_id: Uuid,
    pub algorithm_version: String,
    pub parameter_version: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

impl SignalDefinition {
    pub fn validate_execution_geometry(&self) -> Result<(), SignalExecutionPlanError> {
        let Some(execution_plan) = &self.execution_plan else {
            return Ok(());
        };
        let entry_reference = self
            .projected_entry
            .or_else(|| {
                self.entry_zone
                    .as_ref()
                    .map(|zone| zone.low + (zone.high - zone.low) / Decimal::TWO)
            })
            .ok_or(SignalExecutionPlanError::MissingEntryReference)?;

        execution_plan.validate(self.direction, entry_reference, self.primary_target)?;
        if execution_plan.hard_stop != self.stop_loss {
            return Err(SignalExecutionPlanError::HardStopMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SignalEventKind {
    Watching,
    EntryAvailable,
    Triggered,
    Invalidated,
    Expired,
}

impl SignalEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Watching => "WATCHING",
            Self::EntryAvailable => "ENTRY_AVAILABLE",
            Self::Triggered => "TRIGGERED",
            Self::Invalidated => "INVALIDATED",
            Self::Expired => "EXPIRED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalEvent {
    pub event_id: Uuid,
    pub signal_id: Uuid,
    pub sequence: i64,
    pub kind: SignalEventKind,
    pub effective_at: i64,
    pub recorded_at: i64,
    pub virtual_trade_id: Option<Uuid>,
    pub price: Option<Decimal>,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DirectiveKind {
    MoveStop,
    MoveTarget,
    SuggestClose,
    EmergencyExit,
    ExitAtNextOpen,
    ReverseAtNextOpen,
    CancelEntry,
    ExpireEntry,
}

impl DirectiveKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MoveStop => "MOVE_STOP",
            Self::MoveTarget => "MOVE_TARGET",
            Self::SuggestClose => "SUGGEST_CLOSE",
            Self::EmergencyExit => "EMERGENCY_EXIT",
            Self::ExitAtNextOpen => "EXIT_AT_NEXT_OPEN",
            Self::ReverseAtNextOpen => "REVERSE_AT_NEXT_OPEN",
            Self::CancelEntry => "CANCEL_ENTRY",
            Self::ExpireEntry => "EXPIRE_ENTRY",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementDirective {
    pub directive_id: Uuid,
    pub signal_id: Uuid,
    pub virtual_trade_id: Option<Uuid>,
    pub kind: DirectiveKind,
    pub effective_at: i64,
    pub price: Option<Decimal>,
    pub reason: String,
    pub source_closed_candle_time: i64,
    pub algorithm_version: String,
    pub parameter_version: String,
    /// Present for `ReverseAtNextOpen` when analysis has a validated opposite setup.
    /// Execution uses this to open the reverse trade atomically with the close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_signal: Option<SignalDefinition>,
}
