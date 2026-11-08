#![forbid(unsafe_code)]

pub mod edge;
mod execution;
pub mod hypotheses;
mod metrics;
mod replay;

pub use execution::{
    CandlePriceReference, ExecutionAdapter, ExecutionKind, ExecutionModel,
    OhlcConservativeExecution, SimulatedExecution, SpreadConfig, SpreadConfigError,
};
pub use metrics::{
    AccountSimulationConfig, BacktestDiagnosticObservation, BacktestDiagnostics, BacktestMetrics,
    BacktestTrade, FeeConfig, SlippageConfig, compute_diagnostics, compute_metrics,
};
pub use replay::{
    BacktestReplay, BacktestReplayError, BacktestReplayResult, BacktestReplaySession,
    BacktestRunInput, ChronologicalSplit, ReplayFrame,
};
