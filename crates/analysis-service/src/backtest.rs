use crate::AnalysisServiceError;
pub use backtest_engine::{
    AccountSimulationConfig, BacktestMetrics, BacktestReplayResult, BacktestRunInput,
    BacktestTrade, FeeConfig, SlippageConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunRecord {
    pub run_id: Uuid,
    pub input: BacktestRunInput,
    pub input_json: Value,
    pub code_commit_hash: String,
}

impl BacktestRunRecord {
    pub fn new(run_id: Uuid, input: BacktestRunInput) -> Result<Self, AnalysisServiceError> {
        let input_json = serde_json::to_value(&input)
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        Ok(Self {
            run_id,
            code_commit_hash: input.code_commit_hash.clone(),
            input,
            input_json,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BacktestRunStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl BacktestRunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunView {
    pub run_id: Uuid,
    pub status: BacktestRunStatus,
    pub input: BacktestRunInput,
    pub metrics: Option<Value>,
    pub error: Option<Value>,
    pub trades: Vec<crate::BacktestTradeRecord>,
}
