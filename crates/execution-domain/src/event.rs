use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VirtualTradeEventKind {
    EntryTriggered,
    StopMoved,
    TargetMoved,
    PartialTargetTouched,
    RunnerActivated,
    ExitScheduled,
    TargetTouched,
    StopTouched,
    Reversed,
    RegimeInvalidated,
    TrackingStale,
    Reconciled,
    Closed,
}

impl VirtualTradeEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EntryTriggered => "ENTRY_TRIGGERED",
            Self::StopMoved => "STOP_MOVED",
            Self::TargetMoved => "TARGET_MOVED",
            Self::PartialTargetTouched => "PARTIAL_TARGET_TOUCHED",
            Self::RunnerActivated => "RUNNER_ACTIVATED",
            Self::ExitScheduled => "EXIT_SCHEDULED",
            Self::TargetTouched => "TARGET_TOUCHED",
            Self::StopTouched => "STOP_TOUCHED",
            Self::Reversed => "REVERSED",
            Self::RegimeInvalidated => "REGIME_INVALIDATED",
            Self::TrackingStale => "TRACKING_STALE",
            Self::Reconciled => "RECONCILED",
            Self::Closed => "CLOSED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTradeEvent {
    pub event_id: Uuid,
    pub virtual_trade_id: Uuid,
    pub sequence: i64,
    pub kind: VirtualTradeEventKind,
    pub event_time: i64,
    pub payload: Value,
}
