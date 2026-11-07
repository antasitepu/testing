use crate::{storage_error, storage_sqlx_error};
use analysis_domain::SignalDefinition;
use async_trait::async_trait;
use execution_domain::TrackingHealth;
use execution_service::{
    ExecutionError, MonitoringEventEnvelope, MonitoringSnapshot, MonitoringStore,
    OutboxStreamBatch, signal_recommendation_payload, virtual_trade_payload_with_signal,
};
use runtime_observability::record_subscriber_lag;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct MonitoringRepository {
    pool: PgPool,
}

impl MonitoringRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MonitoringStore for MonitoringRepository {
    async fn snapshot(&self) -> Result<MonitoringSnapshot, ExecutionError> {
        let latest_cursor: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(cursor), 0) FROM monitoring_events")
                .fetch_one(&self.pool)
                .await
                .map_err(|error| storage_sqlx_error("load latest monitoring cursor", error))?;
        let virtual_trades = sqlx::query(
            "SELECT vt.*, s.definition AS signal_definition \
             FROM virtual_trades vt \
             LEFT JOIN signals s ON s.signal_id = vt.signal_id \
             WHERE vt.trade_state = 'OPEN' \
             ORDER BY vt.created_at, vt.virtual_trade_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load monitoring virtual trades", error))?
        .into_iter()
        .map(|row| {
            let signal_definition = row
                .try_get::<Option<Value>, _>("signal_definition")
                .map_err(|error| storage_error("decode monitoring referenced signal", error))?;
            let trade = crate::tracker_repository::row_to_trade(row)?;
            let signal = decode_referenced_signal(trade.signal_id, signal_definition)?;
            Ok(virtual_trade_payload_with_signal(&trade, Some(&signal)))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
        let recommendations = sqlx::query(
            "SELECT DISTINCT ON (s.symbol, s.plan, s.interval) s.definition \
             FROM signals s \
             JOIN tracker_states ts ON ts.signal_id = s.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
             ORDER BY s.symbol, s.plan, s.interval, s.created_at DESC, s.signal_id DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load monitoring recommendations", error))?
        .into_iter()
        .map(|row| {
            let definition: serde_json::Value = row.get("definition");
            let signal = serde_json::from_value(definition)
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
            Ok(signal_recommendation_payload(&signal))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
        let mut health = BTreeMap::new();
        let rows = sqlx::query("SELECT symbol, tracking_health FROM tracker_states")
            .fetch_all(&self.pool)
            .await
            .map_err(|error| storage_sqlx_error("load monitoring health", error))?;
        for row in rows {
            health.insert(
                row.get::<String, _>("symbol"),
                parse_health(&row.get::<String, _>("tracking_health"))?,
            );
        }
        let summaries = load_plan_summaries(&self.pool, &health).await?;
        Ok(MonitoringSnapshot {
            recommendations,
            virtual_trades,
            summaries,
            health,
            latest_cursor,
        })
    }

    async fn events_after(
        &self,
        cursor: i64,
        limit: u32,
    ) -> Result<OutboxStreamBatch, ExecutionError> {
        if limit == 0 || limit > 1_000 {
            return Err(ExecutionError::InvalidData(
                "monitoring event limit must be 1..=1000".to_owned(),
            ));
        }
        let oldest: Option<i64> = sqlx::query_scalar("SELECT MIN(cursor) FROM monitoring_events")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| storage_sqlx_error("load oldest monitoring cursor", error))?;
        if let Some(oldest) = oldest
            && cursor >= 0
            && cursor < oldest - 1
        {
            record_subscriber_lag();
            return Ok(OutboxStreamBatch::resync_required(cursor));
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
        Ok(OutboxStreamBatch::from_events(
            cursor,
            rows.into_iter()
                .map(|row| MonitoringEventEnvelope {
                    cursor: row.get("cursor"),
                    event_id: row.get("event_id"),
                    event_type: normalize_event_type(&row.get::<String, _>("event_type")),
                    entity_type: row.get("entity_type"),
                    entity_id: row.get("entity_id"),
                    payload: row.get("payload"),
                })
                .collect(),
        ))
    }

    async fn mark_published(&self, cursor: i64) -> Result<(), ExecutionError> {
        sqlx::query("UPDATE monitoring_events SET published_at = NOW() WHERE cursor = $1")
            .bind(cursor)
            .execute(&self.pool)
            .await
            .map_err(|error| storage_sqlx_error("mark monitoring event published", error))?;
        Ok(())
    }

    async fn cleanup_published_before(
        &self,
        older_than_ms: i64,
        limit: u32,
    ) -> Result<u64, ExecutionError> {
        let result = sqlx::query(
            "DELETE FROM monitoring_events WHERE cursor IN ( \
                 SELECT cursor FROM monitoring_events \
                 WHERE published_at IS NOT NULL \
                   AND last_publish_error IS NULL \
                   AND created_at < to_timestamp($1 / 1000.0) \
                 ORDER BY cursor LIMIT $2 \
             )",
        )
        .bind(older_than_ms)
        .bind(i64::from(limit.min(500)))
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("cleanup monitoring events", error))?;
        Ok(result.rows_affected())
    }
}

fn decode_referenced_signal(
    signal_id: uuid::Uuid,
    definition: Option<Value>,
) -> Result<SignalDefinition, ExecutionError> {
    let definition = definition.ok_or_else(|| {
        ExecutionError::InvalidData(format!(
            "virtual trade references missing referenced signal {signal_id}"
        ))
    })?;
    serde_json::from_value(definition).map_err(|error| {
        ExecutionError::InvalidData(format!("invalid referenced signal {signal_id}: {error}"))
    })
}

fn normalize_event_type(value: &str) -> String {
    value.to_ascii_lowercase().replace('_', "-")
}

async fn load_plan_summaries(
    pool: &PgPool,
    health: &BTreeMap<String, TrackingHealth>,
) -> Result<Vec<Value>, ExecutionError> {
    let rows = sqlx::query(
        "SELECT prs.symbol, prs.plan, prs.interval, prs.state, prs.last_evaluated_close_time, \
                ts.state->>'setupState' AS tracker_setup_state, \
                ts.state->>'tradeState' AS tracker_trade_state, \
                vt.virtual_trade_id::TEXT AS open_virtual_trade_id \
         FROM plan_runtime_states prs \
         JOIN configuration_releases cr \
           ON cr.release_id = prs.release_id \
          AND cr.symbol = prs.symbol \
          AND cr.lifecycle = 'ACTIVE' \
         LEFT JOIN tracker_states ts \
           ON ts.signal_id::TEXT = prs.state->'activeSignal'->>'signalId' \
         LEFT JOIN virtual_trades vt \
           ON vt.signal_id = ts.signal_id \
          AND vt.trade_state = 'OPEN' \
         ORDER BY prs.symbol, CASE prs.interval \
             WHEN '1m' THEN 1 WHEN '5m' THEN 2 WHEN '15m' THEN 3 \
             WHEN '30m' THEN 4 WHEN '1h' THEN 5 WHEN '4h' THEN 6 \
             WHEN '1d' THEN 7 ELSE 8 END, prs.plan",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| storage_sqlx_error("load monitoring plan summaries", error))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let symbol: String = row.get("symbol");
            let plan: String = row.get("plan");
            let timeframe: String = row.get("interval");
            let state: Value = row.get("state");
            let overlay = normalized_summary_overlay(
                &state,
                row.get("tracker_setup_state"),
                row.get("tracker_trade_state"),
                row.get("open_virtual_trade_id"),
            );
            json!({
                "symbol": symbol,
                "plan": plan,
                "timeframe": timeframe,
                "interval": timeframe,
                "readiness": readiness_label(&state),
                "setupState": overlay.setup_state,
                "tradeState": overlay.trade_state,
                "direction": overlay.direction,
                "trackingHealth": health
                    .get(&symbol)
                    .copied()
                    .map(health_label)
                    .unwrap_or("LIVE"),
                "activeSignalId": overlay.active_signal_id,
                "activeVirtualTradeId": overlay.active_virtual_trade_id,
                "evaluatedClosedCandleTime": row.get::<i64, _>("last_evaluated_close_time"),
            })
        })
        .collect())
}

struct NormalizedSummaryOverlay {
    setup_state: String,
    trade_state: String,
    direction: String,
    active_signal_id: Value,
    active_virtual_trade_id: Value,
}

fn normalized_summary_overlay(
    state: &Value,
    tracker_setup_state: Option<String>,
    tracker_trade_state: Option<String>,
    open_virtual_trade_id: Option<String>,
) -> NormalizedSummaryOverlay {
    let runtime_signal_id = state
        .get("activeSignal")
        .and_then(|value| value.get("signalId"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut overlay = NormalizedSummaryOverlay {
        setup_state: string_field(state, "setupState")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        trade_state: string_field(state, "tradeState")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        direction: string_field(state, "direction")
            .unwrap_or("NONE")
            .to_owned(),
        active_signal_id: runtime_signal_id.clone(),
        active_virtual_trade_id: state
            .get("activeVirtualTradeId")
            .cloned()
            .unwrap_or(Value::Null),
    };
    let has_runtime_signal = !runtime_signal_id.is_null();
    let has_open_trade = open_virtual_trade_id.is_some();
    let has_active_recommendation = tracker_setup_state.as_deref() == Some("ENTRY_AVAILABLE")
        && tracker_trade_state.as_deref() == Some("NOT_ENTERED")
        && !has_open_trade;

    if let Some(trade_id) = open_virtual_trade_id {
        overlay.setup_state = tracker_setup_state.unwrap_or(overlay.setup_state);
        overlay.trade_state = "OPEN".to_owned();
        overlay.active_virtual_trade_id = json!(trade_id);
        return overlay;
    }
    if has_active_recommendation {
        overlay.setup_state = "ENTRY_AVAILABLE".to_owned();
        overlay.trade_state = "NOT_ENTERED".to_owned();
        overlay.active_virtual_trade_id = Value::Null;
        return overlay;
    }
    if has_runtime_signal || is_user_facing_terminal_state(&overlay) {
        overlay.setup_state = "WATCHING".to_owned();
        overlay.trade_state = "NOT_ENTERED".to_owned();
        overlay.direction = "NONE".to_owned();
        overlay.active_signal_id = Value::Null;
        overlay.active_virtual_trade_id = Value::Null;
    }
    overlay
}

fn is_user_facing_terminal_state(overlay: &NormalizedSummaryOverlay) -> bool {
    matches!(
        overlay.setup_state.as_str(),
        "ENTRY_AVAILABLE" | "INVALIDATED" | "EXPIRED"
    ) || matches!(overlay.trade_state.as_str(), "OPEN" | "CLOSED")
}

fn readiness_label(state: &Value) -> &str {
    state
        .get("readiness")
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
}

fn string_field<'a>(state: &'a Value, key: &str) -> Option<&'a str> {
    state.get(key).and_then(Value::as_str)
}

fn health_label(value: TrackingHealth) -> &'static str {
    match value {
        TrackingHealth::Live => "LIVE",
        TrackingHealth::Stale => "STALE",
        TrackingHealth::Reconciling => "RECONCILING",
        TrackingHealth::MarketClosed => "MARKET_CLOSED",
    }
}

fn parse_health(value: &str) -> Result<TrackingHealth, ExecutionError> {
    match value {
        "LIVE" => Ok(TrackingHealth::Live),
        "STALE" => Ok(TrackingHealth::Stale),
        "RECONCILING" => Ok(TrackingHealth::Reconciling),
        "MARKET_CLOSED" => Ok(TrackingHealth::MarketClosed),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown tracking health {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_referenced_signal_is_an_integrity_error() {
        let signal_id = uuid::Uuid::from_u128(7);

        let error = decode_referenced_signal(signal_id, None)
            .expect_err("missing FK target must fail monitoring projection");

        assert!(matches!(error, ExecutionError::InvalidData(_)));
        assert!(error.to_string().contains("missing referenced signal"));
        assert!(error.to_string().contains(&signal_id.to_string()));
    }
}
