use crate::ExecutionError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewScalpingAuditSnapshot {
    pub symbol: String,
    pub mode: String,
    pub timeframes: Vec<String>,
    pub report: Value,
    pub journal: Value,
    pub audit: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalpingAuditSnapshot {
    pub snapshot_id: Uuid,
    pub symbol: String,
    pub mode: String,
    pub timeframes: Vec<String>,
    pub report: Value,
    pub journal: Value,
    pub audit: Value,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalpingAuditSnapshotQuery {
    pub symbol: Option<String>,
    /// Opaque keyset cursor returned as `next_cursor` by the previous page.
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalpingAuditSnapshotPage {
    pub items: Vec<ScalpingAuditSnapshot>,
    pub next_cursor: Option<String>,
}

#[async_trait]
pub trait ScalpingAuditStore: Send + Sync {
    async fn create(
        &self,
        snapshot: NewScalpingAuditSnapshot,
    ) -> Result<ScalpingAuditSnapshot, ExecutionError>;
    async fn list(
        &self,
        query: ScalpingAuditSnapshotQuery,
    ) -> Result<ScalpingAuditSnapshotPage, ExecutionError>;
}
