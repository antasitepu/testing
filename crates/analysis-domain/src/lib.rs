#![forbid(unsafe_code)]

mod config;
mod fact;
mod plan;
mod signal;
mod snapshot;
mod work;

pub use config::{
    HtfFilterMode, IctFvgSelection, IctParameters, IctSweepBiasMode, MaterializedProfile,
    MeanReversionParameters, PlanParameterError, PlanParameters, PlanProfileKey,
    PositionManagementParameters, ProfitProtectionParameters, QuasimodoParameters, ReleaseMode,
    RsiPlanParameters, SessionBreakoutParameters, SmcParameters, TrendBreakoutParameters,
    VersionIdentity, VersionIdentityError, VolatilitySqueezeParameters,
    default_rsi_reversal_confirmation_bars,
};
pub use fact::{
    ConfirmedFact, EvidenceRef, FactId, FactKind, FactLifecycle, PrimitiveVersion, ReferenceLevel,
    TimeframeFacts,
};
pub use plan::{
    CurrentPlanProjection, Direction, ExecutionInstruction, PlanFeedback, PlanKind, Readiness,
    ReadinessReason, SetupState, TrackingHealth, TradeState,
};
pub use signal::{
    DirectiveKind, HtfAlignment, HtfContext, ManagementDirective, PreEntryValidation, PriceZone,
    PriceZoneError, SignalDefinition, SignalEvent, SignalEventKind, SignalExecutionPlan,
    SignalExecutionPlanError, TargetLeg,
};
pub use snapshot::{CanonicalMarketSnapshot, SnapshotError};
pub use work::{
    AnalysisWorkItem, EvaluationIdentity, StructuredAnalysisError, WorkSource, WorkStatus,
    WorkValidationError,
};
