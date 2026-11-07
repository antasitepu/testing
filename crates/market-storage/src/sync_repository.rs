use crate::{StorageError, parse_sync_status, sync_status_text};
use market_domain::{Symbol, SyncStatus, Timeframe};
use market_service::SyncOperation;
use sqlx::{PgPool, Row};

#[derive(Clone)]
pub struct SyncRepository {
    pool: PgPool,
}

impl SyncRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, status: &SyncStatus) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (
                symbol, interval, oldest_synced_time, latest_synced_time,
                full_history_complete, recent_sync_status, backfill_status, last_error
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (symbol, interval) DO UPDATE SET
                oldest_synced_time = EXCLUDED.oldest_synced_time,
                latest_synced_time = EXCLUDED.latest_synced_time,
                full_history_complete = EXCLUDED.full_history_complete,
                recent_sync_status = EXCLUDED.recent_sync_status,
                backfill_status = EXCLUDED.backfill_status,
                last_error = EXCLUDED.last_error,
                updated_at = NOW()
            "#,
        )
        .bind(status.symbol.as_str())
        .bind(status.timeframe.as_str())
        .bind(status.oldest_synced_time)
        .bind(status.latest_synced_time)
        .bind(status.full_history_complete)
        .bind(sync_status_text(status.recent_sync_status))
        .bind(sync_status_text(status.backfill_status))
        .bind(&status.last_error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, StorageError> {
        let row = sqlx::query(
            r#"
            SELECT symbol, interval, oldest_synced_time, latest_synced_time,
                   full_history_complete, recent_sync_status, backfill_status, last_error
            FROM sync_states
            WHERE symbol = $1 AND interval = $2
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_status).transpose()
    }

    pub async fn all_for_symbol(&self, symbol: &Symbol) -> Result<Vec<SyncStatus>, StorageError> {
        sqlx::query(
            r#"
            SELECT symbol, interval, oldest_synced_time, latest_synced_time,
                   full_history_complete, recent_sync_status, backfill_status, last_error
            FROM sync_states
            WHERE symbol = $1
            ORDER BY CASE interval
                WHEN '1m' THEN 1 WHEN '5m' THEN 2 WHEN '15m' THEN 3
                WHEN '30m' THEN 4 WHEN '1h' THEN 5 WHEN '4h' THEN 6
                WHEN '1d' THEN 7
            END
            "#,
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_status)
        .collect()
    }

    pub async fn mark_recent_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (symbol, interval, recent_sync_status)
            VALUES ($1, $2, 'running')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                recent_sync_status = 'running',
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_recent_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
        latest: i64,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (
                symbol, interval, oldest_synced_time, latest_synced_time,
                recent_sync_status
            )
            VALUES ($1, $2, $3, $4, 'complete')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                oldest_synced_time = LEAST(
                    COALESCE(sync_states.oldest_synced_time, EXCLUDED.oldest_synced_time),
                    EXCLUDED.oldest_synced_time
                ),
                latest_synced_time = GREATEST(
                    COALESCE(sync_states.latest_synced_time, EXCLUDED.latest_synced_time),
                    EXCLUDED.latest_synced_time
                ),
                recent_sync_status = 'complete',
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(oldest)
        .bind(latest)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_backfill_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (symbol, interval, backfill_status)
            VALUES ($1, $2, 'running')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                backfill_status = 'running',
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_backfill_progress(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (
                symbol, interval, oldest_synced_time, backfill_status
            )
            VALUES ($1, $2, $3, 'running')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                oldest_synced_time = LEAST(
                    COALESCE(sync_states.oldest_synced_time, EXCLUDED.oldest_synced_time),
                    EXCLUDED.oldest_synced_time
                ),
                backfill_status = 'running',
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(oldest)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_backfill_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO sync_states (
                symbol, interval, full_history_complete, backfill_status
            )
            VALUES ($1, $2, TRUE, 'complete')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                full_history_complete = TRUE,
                backfill_status = 'complete',
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_failed(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        operation: SyncOperation,
        message: &str,
    ) -> Result<(), StorageError> {
        let query = match operation {
            SyncOperation::Recent => {
                r#"
                INSERT INTO sync_states (
                    symbol, interval, recent_sync_status, last_error
                )
                VALUES ($1, $2, 'failed', $3)
                ON CONFLICT (symbol, interval) DO UPDATE SET
                    recent_sync_status = 'failed',
                    last_error = EXCLUDED.last_error,
                    updated_at = NOW()
                "#
            }
            SyncOperation::Backfill => {
                r#"
                INSERT INTO sync_states (
                    symbol, interval, backfill_status, last_error
                )
                VALUES ($1, $2, 'failed', $3)
                ON CONFLICT (symbol, interval) DO UPDATE SET
                    backfill_status = 'failed',
                    last_error = EXCLUDED.last_error,
                    updated_at = NOW()
                "#
            }
        };
        sqlx::query(query)
            .bind(symbol.as_str())
            .bind(timeframe.as_str())
            .bind(message)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn row_to_status(row: sqlx::postgres::PgRow) -> Result<SyncStatus, StorageError> {
    Ok(SyncStatus {
        symbol: Symbol::new(row.try_get::<String, _>("symbol")?)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?,
        timeframe: row
            .try_get::<String, _>("interval")?
            .parse::<Timeframe>()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?,
        oldest_synced_time: row.try_get("oldest_synced_time")?,
        latest_synced_time: row.try_get("latest_synced_time")?,
        full_history_complete: row.try_get("full_history_complete")?,
        recent_sync_status: parse_sync_status(row.try_get("recent_sync_status")?)?,
        backfill_status: parse_sync_status(row.try_get("backfill_status")?)?,
        last_error: row.try_get("last_error")?,
    })
}
