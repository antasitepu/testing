use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringEvent {
    pub cursor: i64,
    pub event_id: Uuid,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub payload: Value,
    pub created_at: i64,
}
