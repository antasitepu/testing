#![forbid(unsafe_code)]

mod common;
mod ict;
mod mean_reversion;
mod quasimodo;
mod rsi;
mod smc;
mod trend_breakout;

pub use common::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState,
    RejectedCandidate, RunnerHealthAction, RunnerHealthEvaluation, RunnerHealthInput, append_event,
    apply_feedback, apply_terminal_event, can_reverse, create_signal, evaluate_runner_health,
    project_state,
};
pub use execution_domain::{PositionStage, TradeHealth};
pub use ict::IctEvaluator;
pub use mean_reversion::{
    MeanReversionEvaluator, PreEntryRejection, validate_mean_reversion_entry,
};
pub use quasimodo::QuasimodoEvaluator;
pub use rsi::{RsiEvaluator, RsiV5Evaluator};
pub use smc::SmcEvaluator;
pub use trend_breakout::TrendBreakoutEvaluator;
