use crate::ExecutionError;
use analysis_domain::{PlanFeedback, SignalDefinition, SignalEvent};
use async_trait::async_trait;
use execution_domain::{
    MarketCursor, MonitoringEvent, TrackingHealth, VirtualTrade, VirtualTradeEvent,
};
use market_domain::Symbol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerState {
    pub signal_id: Uuid,
    pub symbol: Symbol,
    pub state: Value,
    pub tracking_health: TrackingHealth,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerSnapshot {
    pub cursor: Option<MarketCursor>,
    pub active_recommendations: Vec<SignalDefinition>,
    pub active_trades: Vec<VirtualTrade>,
    pub active_trade_sequences: Vec<ActiveTradeSequence>,
    pub tracker_states: Vec<TrackerState>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTradeSequence {
    pub virtual_trade_id: Uuid,
    pub next_sequence: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTradeWrite {
    pub trade: VirtualTrade,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMonitoringEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub payload: Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableTransition {
    pub tracker_state: Option<TrackerState>,
    pub tracker_states: Vec<TrackerState>,
    pub virtual_trade_write: Option<VirtualTradeWrite>,
    pub virtual_trade_writes: Vec<VirtualTradeWrite>,
    pub trade_events: Vec<VirtualTradeEvent>,
    pub signal_events: Vec<SignalEvent>,
    pub plan_feedback: Vec<PlanFeedback>,
    pub consumed_directive_ids: Vec<Uuid>,
    pub market_cursor: Option<MarketCursor>,
    pub monitoring_events: Vec<NewMonitoringEvent>,
    pub inject_failure_after_writes: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthTransition {
    pub state: TrackerState,
    pub monitoring_event: NewMonitoringEvent,
}

#[async_trait]
pub trait TrackerStore: Send + Sync {
    async fn load_active(&self, symbol: &Symbol) -> Result<TrackerSnapshot, ExecutionError>;
    async fn checkpoint_cursor(&self, cursor: MarketCursor) -> Result<(), ExecutionError>;
    async fn commit_transition(
        &self,
        transition: DurableTransition,
    ) -> Result<Option<MonitoringEvent>, ExecutionError>;
    async fn mark_health(
        &self,
        change: HealthTransition,
    ) -> Result<MonitoringEvent, ExecutionError>;
}

impl DurableTransition {
    /// True when the transition must be persisted (trade/state/events), even if
    /// it has no monitoring outbox row (for example MFE/MAE checkpoints).
    /// Folds `other` into this transition so several steps commit atomically.
    pub fn absorb(&mut self, other: DurableTransition) {
        if let Some(state) = other.tracker_state {
            if self.tracker_state.is_none() {
                self.tracker_state = Some(state);
            } else {
                self.tracker_states.push(state);
            }
        }
        self.tracker_states.extend(other.tracker_states);
        if let Some(write) = other.virtual_trade_write {
            if self.virtual_trade_write.is_none() {
                self.virtual_trade_write = Some(write);
            } else {
                self.virtual_trade_writes.push(write);
            }
        }
        self.virtual_trade_writes.extend(other.virtual_trade_writes);
        self.trade_events.extend(other.trade_events);
        self.signal_events.extend(other.signal_events);
        self.plan_feedback.extend(other.plan_feedback);
        self.consumed_directive_ids
            .extend(other.consumed_directive_ids);
        if other.market_cursor.is_some() {
            self.market_cursor = other.market_cursor;
        }
        self.monitoring_events.extend(other.monitoring_events);
        self.inject_failure_after_writes |= other.inject_failure_after_writes;
    }

    pub fn needs_commit(&self) -> bool {
        self.tracker_state.is_some()
            || !self.tracker_states.is_empty()
            || self.virtual_trade_write.is_some()
            || !self.virtual_trade_writes.is_empty()
            || !self.trade_events.is_empty()
            || !self.signal_events.is_empty()
            || !self.plan_feedback.is_empty()
            || !self.consumed_directive_ids.is_empty()
            || !self.monitoring_events.is_empty()
    }
}
