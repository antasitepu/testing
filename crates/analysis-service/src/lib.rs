#![forbid(unsafe_code)]

mod backtest;
mod configuration;
mod error;
mod ports;
mod runtime;
mod scheduler;
mod worker;

pub use analysis_domain::MaterializedProfile;
pub use backtest::{
    AccountSimulationConfig, BacktestMetrics, BacktestReplayResult, BacktestRunInput,
    BacktestRunRecord, BacktestRunStatus, BacktestRunView, BacktestTrade, FeeConfig,
    SlippageConfig,
};
pub use configuration::{ConfigurationValidationError, validate_activation, validate_release};
pub use error::AnalysisServiceError;
pub use ports::{
    AnalysisCandleSource, AnalysisWorkStore, BacktestRunResult, BacktestStore, BacktestTradeRecord,
    ConfigurationRelease, ConfigurationStore, EvaluationCommit, GapStatusSource, MonitoringEvent,
    MonitoringEventRecord, PlanFeedbackStore, PlanRuntimeStateRecord, RejectedCandidateStore,
    RejectedPlanCandidate, RejectedPlanCandidateQuery, ReleaseLifecycle, SignalHistoryQuery,
    SignalPage, SignalStore, SnapshotAvailability, ValidationGate,
};
pub use runtime::PlanRegistry;
pub use worker::{AnalysisWorker, WorkerStep, suppresses_new_rsi_m1_entry};
