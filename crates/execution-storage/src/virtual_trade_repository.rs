use crate::{storage_error, storage_sqlx_error};
use async_trait::async_trait;
use execution_service::{
    ExecutionError, VirtualTradeDetail, VirtualTradeHistoryPage, VirtualTradeHistoryQuery,
    VirtualTradeStore, virtual_trade_payload,
};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct VirtualTradeRepository {
    pool: PgPool,
}

impl VirtualTradeRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl VirtualTradeStore for VirtualTradeRepository {
    async fn history(
        &self,
        query: VirtualTradeHistoryQuery,
    ) -> Result<VirtualTradeHistoryPage, ExecutionError> {
        let rows = sqlx::query(
            "WITH page AS MATERIALIZED ( \
                 SELECT virtual_trades.* \
                 FROM virtual_trades \
                 WHERE ($1::TEXT IS NULL OR symbol = $1) \
                   AND ($2::TEXT IS NULL OR plan = $2) \
                   AND ($3::TEXT IS NULL OR interval = $3) \
                   AND ($4::TEXT IS NULL OR direction = $4) \
                   AND ($5::TEXT IS NULL OR result->>'exitReason' = $5) \
                   AND ($6::BIGINT IS NULL OR actual_entry_time >= $6) \
                   AND ($7::BIGINT IS NULL OR actual_entry_time <= $7) \
                   AND ($8::BIGINT IS NULL OR actual_entry_time < $8) \
                 ORDER BY actual_entry_time DESC \
                 FETCH FIRST $9 ROWS WITH TIES \
             ) \
             SELECT page.*, \
                    EXISTS ( \
                        SELECT 1 \
                        FROM virtual_trades AS older \
                        WHERE ($1::TEXT IS NULL OR older.symbol = $1) \
                          AND ($2::TEXT IS NULL OR older.plan = $2) \
                          AND ($3::TEXT IS NULL OR older.interval = $3) \
                          AND ($4::TEXT IS NULL OR older.direction = $4) \
                          AND ($5::TEXT IS NULL OR older.result->>'exitReason' = $5) \
                          AND ($6::BIGINT IS NULL OR older.actual_entry_time >= $6) \
                          AND ($7::BIGINT IS NULL OR older.actual_entry_time <= $7) \
                          AND ($8::BIGINT IS NULL OR older.actual_entry_time < $8) \
                          AND older.actual_entry_time < ( \
                              SELECT MIN(actual_entry_time) FROM page \
                          ) \
                    ) AS history_has_more \
             FROM page \
             ORDER BY actual_entry_time DESC, virtual_trade_id DESC",
        )
        .bind(query.symbol.as_deref())
        .bind(query.plan.as_deref())
        .bind(query.timeframe.as_deref())
        .bind(query.direction.as_deref())
        .bind(query.exit_reason.as_deref())
        .bind(query.from)
        .bind(query.to)
        .bind(query.cursor)
        .bind(i64::from(query.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load virtual trade history", error))?;
        let next_cursor = rows.last().and_then(|row| {
            row.get::<bool, _>("history_has_more")
                .then(|| row.get("actual_entry_time"))
        });
        Ok(VirtualTradeHistoryPage {
            items: rows
                .into_iter()
                .map(row_to_history_payload)
                .collect::<Result<Vec<_>, _>>()?,
            next_cursor,
        })
    }

    async fn detail(&self, trade_id: Uuid) -> Result<Option<VirtualTradeDetail>, ExecutionError> {
        let Some(trade_row) =
            sqlx::query("SELECT * FROM virtual_trades WHERE virtual_trade_id = $1")
                .bind(trade_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| storage_sqlx_error("load virtual trade detail", error))?
        else {
            return Ok(None);
        };
        let trade = crate::tracker_repository::row_to_trade(trade_row)?;
        let signal_id = trade.signal_id;
        let events = sqlx::query(
            "SELECT sequence, event_type, event_time, payload \
             FROM virtual_trade_events WHERE virtual_trade_id = $1 ORDER BY sequence",
        )
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load virtual trade events", error))?
        .into_iter()
        .map(|row| {
            json!({
                "sequence": row.get::<i64, _>("sequence"),
                "eventType": row.get::<String, _>("event_type"),
                "eventTime": row.get::<i64, _>("event_time"),
                "payload": row.get::<serde_json::Value, _>("payload"),
            })
        })
        .collect();
        let signal = sqlx::query("SELECT definition, evidence FROM signals WHERE signal_id = $1")
            .bind(signal_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| storage_sqlx_error("load virtual trade signal", error))?
            .map(|row| {
                let mut definition: serde_json::Value = row.get("definition");
                definition["evidence"] = row.get("evidence");
                definition
            })
            .unwrap_or_else(|| json!({ "signalId": signal_id, "evidence": [] }));
        let start = trade.actual_entry_time;
        // Open trades have no exit_time. Emit null so clients fetch the latest
        // candle page (before=None) instead of open_time < entry_time, which
        // would exclude the entry candle and all subsequent bars.
        let end = trade.exit_time;
        Ok(Some(VirtualTradeDetail {
            trade: virtual_trade_payload(&trade),
            events,
            signal,
            candle_window: json!({
                "start": start,
                "end": end,
            }),
        }))
    }
}

fn row_to_history_payload(row: sqlx::postgres::PgRow) -> Result<serde_json::Value, ExecutionError> {
    let closed_at = row
        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("closed_at")
        .map_err(|error| storage_error("decode virtual trade closed_at", error))?;
    let trade = crate::tracker_repository::row_to_trade(row)?;
    let mut payload = virtual_trade_payload(&trade);
    payload["closedAt"] = json!(closed_at.map(|value| value.timestamp_millis()));
    Ok(payload)
}
