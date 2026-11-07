#![forbid(unsafe_code)]

mod buffer;
mod error;
mod exact_recovery;
mod history;
mod next_open;
mod outbox;
mod ports;
mod reconciliation;
mod runtime;
mod scalping_audit;
mod supervisor;
mod tracker;
mod zone;

pub use buffer::{BufferError, EffectiveEventBuffer};
pub use error::ExecutionError;
pub use exact_recovery::ExactReplayPlan;
pub use execution_domain::{
    AmbiguityDecision, AmbiguousOutcome, ExecutionKind, OhlcExecutionRange,
    classify_ohlc_ambiguity, known_ordered_outcome,
};
pub use execution_domain::{MarketCursor, MonitoringEvent, TrackingHealth};
pub use history::{
    VirtualTradeDetail, VirtualTradeHistoryPage, VirtualTradeHistoryQuery, VirtualTradeStore,
};
pub use next_open::{NextOpenDecision, NextOpenGuard, PendingNextOpen, PreEntryCancellation};
pub use outbox::{
    MonitoringEventEnvelope, MonitoringSnapshot, MonitoringStore, OutboxDelivery, OutboxRetention,
    OutboxRetentionRow, OutboxStreamBatch,
};
pub use ports::{
    ActiveTradeSequence, DurableTransition, HealthTransition, NewMonitoringEvent, TrackerSnapshot,
    TrackerState, TrackerStore, VirtualTradeWrite,
};
pub use reconciliation::{RecoveryInput, RecoveryOutcome, RecoveryReason, choose_recovery};
pub use runtime::TrackerInput;
pub use scalping_audit::{
    NewScalpingAuditSnapshot, ScalpingAuditSnapshot, ScalpingAuditSnapshotPage,
    ScalpingAuditSnapshotQuery, ScalpingAuditStore,
};
pub use supervisor::{ExecutionSupervisor, SupervisorEvent, SupervisorStep};
pub use tracker::{
    TrackerRuntime, TrackerRuntimeCheckpoint, TrackerRuntimeError, signal_recommendation_payload,
    virtual_trade_payload, virtual_trade_payload_with_signal,
};
pub use zone::{
    LimitZoneDecision, LimitZoneError, LimitZoneGuard, proves_effective_zone_touch,
    proves_zone_touch,
};
