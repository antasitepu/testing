use crate::storage_sqlx_error;
use analysis_domain::{
    CurrentPlanProjection, PlanFeedback, PlanProfileKey, SignalDefinition, SignalEvent,
};
use analysis_plans::{PlanRuntimeState, project_state};
use analysis_service::{
    AnalysisServiceError, MonitoringEvent, MonitoringEventRecord, PlanFeedbackStore,
    SignalHistoryQuery, SignalPage, SignalStore,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

const MAX_POSTGRES_TIMESTAMP_MS: i64 = 253_402_300_799_999;

#[derive(Clone)]
pub struct SignalRepository {
    pool: PgPool,
}

impl SignalRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SignalStore for SignalRepository {
    async fn active_projection(
        &self,
        key: &PlanProfileKey,
    ) -> Result<CurrentPlanProjection, AnalysisServiceError> {
        let row = sqlx::query(
            "SELECT s.state, s.last_evaluated_close_time \
             FROM plan_runtime_states s \
             JOIN configuration_releases r ON r.release_id = s.release_id \
             WHERE s.symbol = $1 AND s.plan = $2 AND s.interval = $3 \
               AND r.mode = 'PAPER_ACTIVE' AND r.lifecycle = 'ACTIVE'",
        )
        .bind(key.symbol.as_str())
        .bind(key.plan.as_str())
        .bind(key.target_timeframe.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load active plan projection", error))?;
        let row = row.ok_or_else(|| {
            AnalysisServiceError::NotFound(format!(
                "{}:{}:{}",
                key.symbol,
                key.plan.as_str(),
                key.target_timeframe
            ))
        })?;
        let state: PlanRuntimeState = serde_json::from_value(row.get("state"))
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        Ok(project_state(&state, row.get("last_evaluated_close_time")))
    }

    async fn history(&self, query: SignalHistoryQuery) -> Result<SignalPage, AnalysisServiceError> {
        if query.limit == 0 || query.limit > 1_000 {
            return Err(AnalysisServiceError::InvalidData(
                "signal history limit must be 1..=1000".to_owned(),
            ));
        }
        let before = query.before_created_at.unwrap_or(MAX_POSTGRES_TIMESTAMP_MS);
        let rows = sqlx::query(
            "SELECT definition, created_at FROM signals \
             WHERE symbol = $1 AND plan = $2 AND interval = $3 \
               AND created_at < to_timestamp($4 / 1000.0) \
             ORDER BY created_at DESC, signal_id DESC LIMIT $5",
        )
        .bind(query.key.symbol.as_str())
        .bind(query.key.plan.as_str())
        .bind(query.key.target_timeframe.as_str())
        .bind(before)
        .bind(i64::from(query.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load signal history", error))?;
        let mut items = Vec::with_capacity(rows.len());
        let mut next_before_created_at = None;
        for row in rows {
            let definition: serde_json::Value = row.get("definition");
            items.push(
                serde_json::from_value::<SignalDefinition>(definition)
                    .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
            );
            next_before_created_at =
                Some(row.get::<DateTime<Utc>, _>("created_at").timestamp_millis());
        }
        Ok(SignalPage {
            items,
            next_before_created_at,
        })
    }

    async fn monitoring_events_after(
        &self,
        cursor: i64,
        limit: u32,
    ) -> Result<Vec<MonitoringEventRecord>, AnalysisServiceError> {
        if cursor < 0 || limit == 0 || limit > 1_000 {
            return Err(AnalysisServiceError::InvalidData(
                "monitoring cursor must be non-negative and limit 1..=1000".to_owned(),
            ));
        }
        let rows = sqlx::query(
            "SELECT cursor, event_id, event_type, entity_type, entity_id, payload \
             FROM monitoring_events WHERE cursor > $1 ORDER BY cursor LIMIT $2",
        )
        .bind(cursor)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load monitoring events", error))?;
        Ok(rows
            .into_iter()
            .map(|row| MonitoringEventRecord {
                cursor: row.get("cursor"),
                event: MonitoringEvent {
                    event_id: row.get("event_id"),
                    event_type: row.get("event_type"),
                    entity_type: row.get("entity_type"),
                    entity_id: row.get("entity_id"),
                    payload: row.get("payload"),
                },
            })
            .collect())
    }
}

#[async_trait]
impl PlanFeedbackStore for SignalRepository {
    async fn pending_feedback(
        &self,
        signal_id: uuid::Uuid,
    ) -> Result<Vec<PlanFeedback>, AnalysisServiceError> {
        let rows = sqlx::query(
            "SELECT payload FROM plan_feedback \
             WHERE signal_id = $1 AND consumed_at IS NULL \
             ORDER BY created_at, feedback_id",
        )
        .bind(signal_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load pending plan feedback", error))?;
        rows.into_iter()
            .map(|row| {
                let payload: serde_json::Value = row.get("payload");
                serde_json::from_value(payload)
                    .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))
            })
            .collect()
    }

    async fn terminal_signal_events(
        &self,
        signal_id: uuid::Uuid,
    ) -> Result<Vec<SignalEvent>, AnalysisServiceError> {
        let rows = sqlx::query(
            "SELECT payload FROM signal_events \
             WHERE signal_id = $1 AND event_type IN ('INVALIDATED', 'EXPIRED') \
             ORDER BY sequence, created_at, event_id",
        )
        .bind(signal_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load pending signal events", error))?;
        rows.into_iter()
            .map(|row| {
                let payload: serde_json::Value = row.get("payload");
                serde_json::from_value(payload)
                    .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))
            })
            .collect()
    }
}
