use crate::PlanKind;
use market_domain::{Symbol, Timeframe};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkSource {
    Live,
    CatchUp,
    DowntimeReconciled,
}

impl WorkSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::CatchUp => "CATCH_UP",
            Self::DowntimeReconciled => "DOWNTIME_RECONCILED",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkStatus {
    Pending,
    Leased,
    Completed,
    Failed,
}

impl WorkStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Leased => "LEASED",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredAnalysisError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisWorkItem {
    pub work_id: Uuid,
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub canonical_close_time: i64,
    pub release_id: Uuid,
    pub source: WorkSource,
    pub status: WorkStatus,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub attempt_count: u32,
    pub error: Option<StructuredAnalysisError>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

impl AnalysisWorkItem {
    pub fn validate(&self) -> Result<(), WorkValidationError> {
        for value in [
            self.canonical_close_time,
            self.created_at,
            self.lease_expires_at.unwrap_or(0),
            self.completed_at.unwrap_or(0),
        ] {
            if value < 0 {
                return Err(WorkValidationError::NegativeTime);
            }
        }
        match self.status {
            WorkStatus::Leased
                if self.lease_owner.as_deref().is_none_or(str::is_empty)
                    || self.lease_expires_at.is_none() =>
            {
                Err(WorkValidationError::InvalidLease)
            }
            WorkStatus::Completed if self.completed_at.is_none() => {
                Err(WorkValidationError::MissingCompletionTime)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationIdentity {
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub target_timeframe: Timeframe,
    pub release_id: Uuid,
    pub algorithm_version: String,
    pub parameter_version: String,
    pub evaluated_closed_candle_time: i64,
}

impl EvaluationIdentity {
    pub fn validate(&self) -> Result<(), WorkValidationError> {
        if self.evaluated_closed_candle_time < 0 {
            return Err(WorkValidationError::NegativeTime);
        }
        if self.algorithm_version.trim().is_empty() {
            return Err(WorkValidationError::BlankAlgorithmVersion);
        }
        if self.parameter_version.trim().is_empty() {
            return Err(WorkValidationError::BlankParameterVersion);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkValidationError {
    #[error("analysis work timestamps must not be negative")]
    NegativeTime,
    #[error("leased work requires an owner and lease expiry")]
    InvalidLease,
    #[error("completed work requires a completion time")]
    MissingCompletionTime,
    #[error("algorithm version must not be blank")]
    BlankAlgorithmVersion,
    #[error("parameter version must not be blank")]
    BlankParameterVersion,
}
