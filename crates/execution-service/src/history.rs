use crate::ExecutionError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTradeHistoryQuery {
    pub symbol: Option<String>,
    pub plan: Option<String>,
    pub timeframe: Option<String>,
    pub direction: Option<String>,
    pub exit_reason: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub cursor: Option<i64>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTradeHistoryPage {
    pub items: Vec<Value>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTradeDetail {
    pub trade: Value,
    pub events: Vec<Value>,
    pub signal: Value,
    pub candle_window: Value,
}

#[async_trait]
pub trait VirtualTradeStore: Send + Sync {
    async fn history(
        &self,
        query: VirtualTradeHistoryQuery,
    ) -> Result<VirtualTradeHistoryPage, ExecutionError>;
    async fn detail(&self, trade_id: Uuid) -> Result<Option<VirtualTradeDetail>, ExecutionError>;
}
