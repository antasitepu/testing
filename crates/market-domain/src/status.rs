use crate::{Symbol, Timeframe};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LiveStatus {
    Syncing,
    Ready,
    Degraded,
    MarketClosed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRunStatus {
    Pending,
    Running,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncStatus {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub oldest_synced_time: Option<i64>,
    pub latest_synced_time: Option<i64>,
    pub full_history_complete: bool,
    pub recent_sync_status: SyncRunStatus,
    pub backfill_status: SyncRunStatus,
    pub last_error: Option<String>,
}
