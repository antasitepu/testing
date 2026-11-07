use crate::AnalysisServiceError;
use crate::{BacktestRunRecord, BacktestRunView};
use analysis_domain::{
    AnalysisWorkItem, CanonicalMarketSnapshot, CurrentPlanProjection, EvaluationIdentity,
    ManagementDirective, MaterializedProfile, PlanFeedback, PlanProfileKey, ReleaseMode,
    SignalDefinition, SignalEvent, StructuredAnalysisError,
};
use async_trait::async_trait;
use market_domain::{Symbol, Timeframe};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReleaseLifecycle {
    Draft,
    Active,
    Retired,
}

impl ReleaseLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Active => "ACTIVE",
            Self::Retired => "RETIRED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationRelease {
    pub release_id: Uuid,
    pub symbol: Symbol,
    pub mode: ReleaseMode,
    pub lifecycle: ReleaseLifecycle,
    pub validation_summary: Value,
    pub profiles: Vec<MaterializedProfile>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationGate {
    pub chronological_split: bool,
    pub out_of_sample_complete: bool,
    pub sample_size: u64,
    pub minimum_sample_size: u64,
    pub unresolved_gap_count: u64,
    pub metrics: Value,
    pub evaluation_start: i64,
    pub evaluation_end: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRuntimeStateRecord {
    pub key: PlanProfileKey,
    pub release_id: Uuid,
    pub state: Value,
    pub last_evaluated_close_time: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedPlanCandidate {
    pub candidate_id: Uuid,
    pub key: PlanProfileKey,
    pub reason: String,
    pub payload: Value,
    pub evaluated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedPlanCandidateQuery {
    pub symbol: Symbol,
    pub timeframes: Vec<Timeframe>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub payload: Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationCommit {
    pub work_id: Uuid,
    /// Lease fencing token: the work item's `attempt_count` when it was leased.
    pub lease_attempt: u32,
    pub evaluations: Vec<EvaluationIdentity>,
    pub runtime_states: Vec<PlanRuntimeStateRecord>,
    pub signals: Vec<SignalDefinition>,
    pub signal_events: Vec<SignalEvent>,
    pub directives: Vec<ManagementDirective>,
    pub consumed_feedback: Vec<PlanFeedback>,
    pub rejected_candidates: Vec<RejectedPlanCandidate>,
    pub monitoring_events: Vec<MonitoringEvent>,
    pub inject_failure_after_writes: bool,
}

#[async_trait]
pub trait PlanFeedbackStore: Send + Sync {
    async fn pending_feedback(
        &self,
        signal_id: Uuid,
    ) -> Result<Vec<PlanFeedback>, AnalysisServiceError>;
    async fn terminal_signal_events(
        &self,
        signal_id: Uuid,
    ) -> Result<Vec<SignalEvent>, AnalysisServiceError>;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalHistoryQuery {
    pub key: PlanProfileKey,
    pub before_created_at: Option<i64>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalPage {
    pub items: Vec<SignalDefinition>,
    pub next_before_created_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringEventRecord {
    pub cursor: i64,
    pub event: MonitoringEvent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestTradeRecord {
    pub trade_id: Uuid,
    pub sequence: i64,
    pub trade: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunResult {
    pub run_id: Uuid,
    pub metrics: Value,
    pub trades: Vec<BacktestTradeRecord>,
}

#[async_trait]
pub trait ConfigurationStore: Send + Sync {
    async fn create_draft(
        &self,
        release: &ConfigurationRelease,
    ) -> Result<(), AnalysisServiceError>;
    async fn activate(
        &self,
        release_id: Uuid,
        gate: &ValidationGate,
    ) -> Result<(), AnalysisServiceError>;
    async fn active_profiles(
        &self,
        symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError>;
    async fn release_mode(&self, release_id: Uuid) -> Result<ReleaseMode, AnalysisServiceError>;
    async fn profiles_for_release(
        &self,
        release_id: Uuid,
        symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError>;
}

#[async_trait]
pub trait AnalysisWorkStore: Send + Sync {
    async fn lease_next(
        &self,
        owner: &str,
        now_ms: i64,
    ) -> Result<Option<AnalysisWorkItem>, AnalysisServiceError>;
    async fn complete_evaluation(
        &self,
        commit: EvaluationCommit,
    ) -> Result<(), AnalysisServiceError>;
    async fn runtime_state(
        &self,
        key: &PlanProfileKey,
        release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError>;
    async fn defer(&self, work_id: Uuid, lease_attempt: u32) -> Result<(), AnalysisServiceError>;
    async fn fail(
        &self,
        work_id: Uuid,
        lease_attempt: u32,
        error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotAvailability {
    Ready(CanonicalMarketSnapshot),
    Waiting,
    Reconcile,
}

#[async_trait]
pub trait AnalysisCandleSource: Send + Sync {
    async fn snapshot(
        &self,
        work: &AnalysisWorkItem,
        profile: &MaterializedProfile,
    ) -> Result<SnapshotAvailability, AnalysisServiceError>;
}

#[async_trait]
pub trait GapStatusSource: Send + Sync {
    async fn has_unresolved_gap(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        close_time: i64,
    ) -> Result<bool, AnalysisServiceError>;
    async fn request_reconciliation(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        close_time: i64,
    ) -> Result<(), AnalysisServiceError>;
}

#[async_trait]
pub trait SignalStore: Send + Sync {
    async fn active_projection(
        &self,
        key: &PlanProfileKey,
    ) -> Result<CurrentPlanProjection, AnalysisServiceError>;
    async fn history(&self, query: SignalHistoryQuery) -> Result<SignalPage, AnalysisServiceError>;
    async fn monitoring_events_after(
        &self,
        cursor: i64,
        limit: u32,
    ) -> Result<Vec<MonitoringEventRecord>, AnalysisServiceError>;
}

#[async_trait]
pub trait RejectedCandidateStore: Send + Sync {
    async fn recent_rejections(
        &self,
        query: RejectedPlanCandidateQuery,
    ) -> Result<Vec<RejectedPlanCandidate>, AnalysisServiceError>;
}

#[async_trait]
pub trait BacktestStore: Send + Sync {
    async fn create_run(&self, input: BacktestRunRecord) -> Result<Uuid, AnalysisServiceError>;
    async fn complete_run(&self, result: BacktestRunResult) -> Result<(), AnalysisServiceError>;
    async fn fail_run(
        &self,
        run_id: Uuid,
        error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError>;
    async fn run(&self, run_id: Uuid) -> Result<Option<BacktestRunView>, AnalysisServiceError>;
}
