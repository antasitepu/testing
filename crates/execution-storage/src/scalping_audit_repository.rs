use crate::storage_sqlx_error;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_service::{
    ExecutionError, NewScalpingAuditSnapshot, ScalpingAuditSnapshot, ScalpingAuditSnapshotPage,
    ScalpingAuditSnapshotQuery, ScalpingAuditStore,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct ScalpingAuditRepository {
    pool: PgPool,
}

impl ScalpingAuditRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ScalpingAuditStore for ScalpingAuditRepository {
    async fn create(
        &self,
        snapshot: NewScalpingAuditSnapshot,
    ) -> Result<ScalpingAuditSnapshot, ExecutionError> {
        let row = sqlx::query(
            "INSERT INTO scalping_audit_snapshots \
             (snapshot_id, symbol, mode, timeframes, report, journal, audit) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING snapshot_id, symbol, mode, timeframes, report, journal, audit, created_at",
        )
        .bind(Uuid::now_v7())
        .bind(snapshot.symbol)
        .bind(snapshot.mode)
        .bind(snapshot.timeframes)
        .bind(snapshot.report)
        .bind(snapshot.journal)
        .bind(snapshot.audit)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("create scalping audit snapshot", error))?;
        Ok(row_to_snapshot(row))
    }

    async fn list(
        &self,
        query: ScalpingAuditSnapshotQuery,
    ) -> Result<ScalpingAuditSnapshotPage, ExecutionError> {
        // Keyset on (created_at, snapshot_id) at full precision so rows sharing a
        // timestamp are neither skipped nor repeated across pages.
        let cursor = query.cursor.as_deref().map(decode_cursor).transpose()?;
        let page_limit = i64::from(query.limit);
        let rows = sqlx::query(
            "SELECT snapshot_id, symbol, mode, timeframes, report, journal, audit, created_at \
             FROM scalping_audit_snapshots \
             WHERE ($1::TEXT IS NULL OR symbol = $1) \
               AND ($2::TIMESTAMPTZ IS NULL OR (created_at, snapshot_id) < ($2, $3)) \
             ORDER BY created_at DESC, snapshot_id DESC LIMIT $4",
        )
        .bind(query.symbol.as_deref())
        .bind(cursor.map(|(created_at, _)| created_at))
        .bind(cursor.map(|(_, snapshot_id)| snapshot_id))
        .bind(page_limit + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("list scalping audit snapshots", error))?;
        let has_more = rows.len() > query.limit as usize;
        let next_cursor = has_more
            .then(|| rows.get(query.limit as usize - 1).map(encode_cursor))
            .flatten();
        let items = rows
            .into_iter()
            .take(query.limit as usize)
            .map(row_to_snapshot)
            .collect::<Vec<_>>();
        Ok(ScalpingAuditSnapshotPage { items, next_cursor })
    }
}

fn encode_cursor(row: &sqlx::postgres::PgRow) -> String {
    let created_at: DateTime<Utc> = row.get("created_at");
    let snapshot_id: Uuid = row.get("snapshot_id");
    format!("{}_{snapshot_id}", created_at.timestamp_micros())
}

fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, Uuid), ExecutionError> {
    cursor
        .split_once('_')
        .and_then(|(micros, snapshot_id)| {
            Some((
                DateTime::<Utc>::from_timestamp_micros(micros.parse().ok()?)?,
                Uuid::parse_str(snapshot_id).ok()?,
            ))
        })
        .ok_or_else(|| ExecutionError::InvalidData("invalid scalping audit cursor".to_owned()))
}

fn row_to_snapshot(row: sqlx::postgres::PgRow) -> ScalpingAuditSnapshot {
    let created_at: DateTime<Utc> = row.get("created_at");
    ScalpingAuditSnapshot {
        snapshot_id: row.get("snapshot_id"),
        symbol: row.get("symbol"),
        mode: row.get("mode"),
        timeframes: row.get("timeframes"),
        report: row.get("report"),
        journal: row.get("journal"),
        audit: row.get("audit"),
        created_at_ms: created_at.timestamp_millis(),
    }
}
