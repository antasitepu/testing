use crate::ExecutionError;
use async_trait::async_trait;
use execution_domain::TrackingHealth;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringSnapshot {
    pub recommendations: Vec<Value>,
    pub virtual_trades: Vec<Value>,
    pub summaries: Vec<Value>,
    pub health: BTreeMap<String, TrackingHealth>,
    pub latest_cursor: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringEventEnvelope {
    pub cursor: i64,
    pub event_id: Uuid,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxStreamBatch {
    pub events: Vec<MonitoringEventEnvelope>,
    pub resync_required: bool,
}

impl OutboxStreamBatch {
    pub fn from_events(_after_cursor: i64, mut events: Vec<MonitoringEventEnvelope>) -> Self {
        events.sort_by_key(|event| event.cursor);
        Self {
            events,
            resync_required: false,
        }
    }

    pub fn resync_required(cursor: i64) -> Self {
        Self {
            events: vec![MonitoringEventEnvelope {
                cursor,
                event_id: Uuid::now_v7(),
                event_type: "resync-required".to_owned(),
                entity_type: "monitoring".to_owned(),
                entity_id: None,
                payload: serde_json::json!({ "cursor": cursor }),
            }],
            resync_required: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutboxDelivery {
    events: Vec<MonitoringEventEnvelope>,
    offered: BTreeSet<i64>,
}

impl OutboxDelivery {
    pub fn new(events: Vec<MonitoringEventEnvelope>) -> Self {
        Self {
            events,
            offered: BTreeSet::new(),
        }
    }

    pub fn mark_offered(&mut self, cursor: i64) {
        if self.events.iter().any(|event| event.cursor == cursor) {
            self.offered.insert(cursor);
        }
    }

    pub fn published_cursors(&self) -> Vec<i64> {
        self.offered.iter().copied().collect()
    }

    pub fn pending_cursors(&self) -> Vec<i64> {
        self.events
            .iter()
            .filter(|event| !self.offered.contains(&event.cursor))
            .map(|event| event.cursor)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxRetentionRow {
    pub cursor: i64,
    pub created_at_ms: i64,
    pub published_at_ms: Option<i64>,
    pub publish_attempts: i32,
    pub last_publish_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutboxRetention {
    retention_days: i64,
    batch_limit: usize,
}

impl OutboxRetention {
    pub const fn new(retention_days: i64, batch_limit: usize) -> Self {
        Self {
            retention_days,
            batch_limit,
        }
    }

    pub const fn cutoff_ms(&self) -> i64 {
        self.retention_days * 24 * 60 * 60 * 1_000
    }

    pub fn row(
        &self,
        cursor: i64,
        created_at_ms: i64,
        published_at_ms: Option<i64>,
        publish_attempts: i32,
        last_publish_error: Option<String>,
    ) -> OutboxRetentionRow {
        OutboxRetentionRow {
            cursor,
            created_at_ms,
            published_at_ms,
            publish_attempts,
            last_publish_error,
        }
    }

    pub fn deletable_cursors(&self, rows: &[OutboxRetentionRow]) -> Vec<i64> {
        rows.iter()
            .filter(|row| row.published_at_ms.is_some())
            .filter(|row| row.last_publish_error.is_none())
            .filter(|row| row.created_at_ms < self.cutoff_ms())
            .take(self.batch_limit)
            .map(|row| row.cursor)
            .collect()
    }
}

#[async_trait]
pub trait MonitoringStore: Send + Sync {
    async fn snapshot(&self) -> Result<MonitoringSnapshot, ExecutionError>;
    async fn events_after(
        &self,
        cursor: i64,
        limit: u32,
    ) -> Result<OutboxStreamBatch, ExecutionError>;
    async fn mark_published(&self, cursor: i64) -> Result<(), ExecutionError>;
    async fn cleanup_published_before(
        &self,
        older_than_ms: i64,
        limit: u32,
    ) -> Result<u64, ExecutionError>;
}
