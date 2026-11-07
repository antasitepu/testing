use crate::{StorageCheckpoint, StorageError, StorageSyncOperation, parse_session_status};
use market_domain::{Candle, ClosedCandleWrite, MarketSession, Symbol, Timeframe};
use market_service::{CandleSessionClass, TradableCandlePolicy};
use sqlx::{PgConnection, PgPool, Row};

#[derive(Clone)]
pub struct CandleRepository {
    pool: PgPool,
}

impl CandleRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert_page(&self, candles: &[Candle]) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await?;
        for candle in candles {
            upsert_candle(&mut transaction, candle).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        checkpoint: &StorageCheckpoint,
    ) -> Result<(), StorageError> {
        validate_page_key(candles, checkpoint)?;
        let mut transaction = self.pool.begin().await?;
        for candle in candles {
            upsert_candle(&mut transaction, candle).await?;
        }
        update_checkpoint(&mut transaction, checkpoint).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn persist_closed_operational(
        &self,
        write: &ClosedCandleWrite,
    ) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await?;
        persist_closed_in(&mut transaction, write).await?;
        transaction.commit().await?;
        Ok(())
    }

    /// One transaction per page instead of one per candle during catch-up.
    pub async fn persist_closed_page_with_checkpoint(
        &self,
        writes: &[ClosedCandleWrite],
        checkpoint: &StorageCheckpoint,
    ) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await?;
        for write in writes {
            persist_closed_in(&mut transaction, write).await?;
        }
        update_checkpoint(&mut transaction, checkpoint).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn latest(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, StorageError> {
        if !(1..=5_000).contains(&limit) {
            return Err(StorageError::InvalidData(
                "candle limit must be between 1 and 5000".to_owned(),
            ));
        }
        sqlx::query(
            r#"
            SELECT symbol, interval, open_time, close_time, open, high, low, close
            FROM (
                SELECT symbol, interval, open_time, close_time, open, high, low, close
                FROM candles
                WHERE symbol = $1
                  AND interval = $2
                  AND ($3::BIGINT IS NULL OR open_time < $3)
                ORDER BY open_time DESC
                LIMIT $4
            ) AS recent
            ORDER BY open_time ASC
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_candle)
        .collect()
    }

    pub async fn bounds(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, StorageError> {
        let row = sqlx::query(
            r#"
            SELECT MIN(open_time) AS oldest, MAX(open_time) AS latest
            FROM candles
            WHERE symbol = $1 AND interval = $2
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .fetch_one(&self.pool)
        .await?;
        let oldest: Option<i64> = row.try_get("oldest")?;
        let latest: Option<i64> = row.try_get("latest")?;
        Ok(oldest.zip(latest))
    }

    pub async fn open_times(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<Vec<i64>, StorageError> {
        if end <= start {
            return Err(StorageError::InvalidData(
                "open-time range end must be later than start".to_owned(),
            ));
        }
        Ok(sqlx::query(
            r#"
            SELECT open_time
            FROM candles
            WHERE symbol = $1
              AND interval = $2
              AND open_time >= $3
              AND open_time < $4
            ORDER BY open_time ASC
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| row.get("open_time"))
        .collect())
    }
}

async fn candle_is_tradable(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candle: &Candle,
) -> Result<bool, StorageError> {
    let end = candle
        .close_time
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidData("candle session range overflowed".to_owned()))?;
    let rows = sqlx::query(
        "SELECT s.session_market, s.session_status, s.session_start_time, \
                s.session_end_time, s.source_event_time \
         FROM market_sessions s \
         JOIN markets m ON m.session_market = s.session_market \
         WHERE m.symbol = $1 \
           AND s.session_end_time > $2 \
           AND s.session_start_time < $3 \
         ORDER BY s.session_start_time, s.source_event_time",
    )
    .bind(candle.symbol.as_str())
    .bind(candle.open_time)
    .bind(end)
    .fetch_all(&mut **transaction)
    .await?;
    let sessions = rows
        .into_iter()
        .map(|row| {
            MarketSession::new(
                row.get::<String, _>("session_market"),
                parse_session_status(row.get("session_status"))?,
                row.get("session_start_time"),
                row.get("session_end_time"),
                row.get("source_event_time"),
            )
            .map_err(|error| StorageError::InvalidData(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TradableCandlePolicy.classify(candle, &sessions) == CandleSessionClass::Tradable)
}

async fn persist_closed_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    write: &ClosedCandleWrite,
) -> Result<(), StorageError> {
    let candle = &write.candle;
    upsert_candle(transaction, candle).await?;
    sqlx::query(
        r#"
            INSERT INTO sync_states (
                symbol, interval, latest_synced_time, recent_sync_status
            )
            VALUES ($1, $2, $3, 'complete')
            ON CONFLICT (symbol, interval) DO UPDATE SET
                latest_synced_time = GREATEST(
                    sync_states.latest_synced_time,
                    EXCLUDED.latest_synced_time
                ),
                recent_sync_status = 'complete',
                last_error = NULL,
                updated_at = NOW()
            "#,
    )
    .bind(candle.symbol.as_str())
    .bind(candle.timeframe.as_str())
    .bind(candle.open_time)
    .execute(&mut **transaction)
    .await?;
    if let Some(source) = write.source.work_source()
        && candle_is_tradable(transaction, candle).await?
    {
        let runtime = sqlx::query(
            "SELECT live_analysis_start_at, active_release_id \
                 FROM analysis_runtime_state WHERE symbol = $1 FOR SHARE",
        )
        .bind(candle.symbol.as_str())
        .fetch_optional(&mut **transaction)
        .await?;
        if let Some(runtime) = runtime {
            let live_analysis_start_at: i64 = runtime.get("live_analysis_start_at");
            let active_release_id: Option<uuid::Uuid> = runtime.get("active_release_id");
            if candle.close_time >= live_analysis_start_at
                && let Some(release_id) = active_release_id
            {
                sqlx::query(
                    "INSERT INTO analysis_work_items \
                         (work_id, symbol, interval, close_time, release_id, source, status) \
                         VALUES ($1, $2, $3, $4, $5, $6, 'PENDING') \
                         ON CONFLICT (symbol, interval, close_time, release_id) DO NOTHING",
                )
                .bind(uuid::Uuid::now_v7())
                .bind(candle.symbol.as_str())
                .bind(candle.timeframe.as_str())
                .bind(candle.close_time)
                .bind(release_id)
                .bind(source)
                .execute(&mut **transaction)
                .await?;
            }
        }
    }
    Ok(())
}

async fn upsert_candle(connection: &mut PgConnection, candle: &Candle) -> Result<(), StorageError> {
    sqlx::query(
        r#"
        INSERT INTO candles (
            symbol, interval, open_time, close_time, open, high, low, close
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (symbol, interval, open_time) DO UPDATE SET
            close_time = EXCLUDED.close_time,
            open = EXCLUDED.open,
            high = EXCLUDED.high,
            low = EXCLUDED.low,
            close = EXCLUDED.close,
            updated_at = NOW()
        "#,
    )
    .bind(candle.symbol.as_str())
    .bind(candle.timeframe.as_str())
    .bind(candle.open_time)
    .bind(candle.close_time)
    .bind(candle.open)
    .bind(candle.high)
    .bind(candle.low)
    .bind(candle.close)
    .execute(connection)
    .await?;
    Ok(())
}

async fn update_checkpoint(
    connection: &mut PgConnection,
    checkpoint: &StorageCheckpoint,
) -> Result<(), StorageError> {
    let (recent_status, backfill_status) = match checkpoint.operation {
        StorageSyncOperation::Recent => (
            if checkpoint.complete {
                "complete"
            } else {
                "running"
            },
            "pending",
        ),
        StorageSyncOperation::Backfill => (
            "pending",
            if checkpoint.complete {
                "complete"
            } else {
                "running"
            },
        ),
    };
    sqlx::query(
        r#"
        INSERT INTO sync_states (
            symbol, interval, oldest_synced_time, latest_synced_time,
            full_history_complete, recent_sync_status, backfill_status
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (symbol, interval) DO UPDATE SET
            oldest_synced_time = LEAST(
                COALESCE(sync_states.oldest_synced_time, EXCLUDED.oldest_synced_time),
                EXCLUDED.oldest_synced_time
            ),
            latest_synced_time = GREATEST(
                COALESCE(sync_states.latest_synced_time, EXCLUDED.latest_synced_time),
                EXCLUDED.latest_synced_time
            ),
            full_history_complete = sync_states.full_history_complete
                OR EXCLUDED.full_history_complete,
            recent_sync_status = CASE
                WHEN $8 = 'recent' THEN EXCLUDED.recent_sync_status
                ELSE sync_states.recent_sync_status
            END,
            backfill_status = CASE
                WHEN $8 = 'backfill' THEN EXCLUDED.backfill_status
                ELSE sync_states.backfill_status
            END,
            last_error = NULL,
            updated_at = NOW()
        "#,
    )
    .bind(checkpoint.symbol.as_str())
    .bind(checkpoint.timeframe.as_str())
    .bind(checkpoint.oldest_synced_time)
    .bind(checkpoint.latest_synced_time)
    .bind(checkpoint.complete && checkpoint.operation == StorageSyncOperation::Backfill)
    .bind(recent_status)
    .bind(backfill_status)
    .bind(checkpoint.operation.as_str())
    .execute(connection)
    .await?;
    Ok(())
}

fn validate_page_key(
    candles: &[Candle],
    checkpoint: &StorageCheckpoint,
) -> Result<(), StorageError> {
    if candles.iter().any(|candle| {
        candle.symbol != checkpoint.symbol || candle.timeframe != checkpoint.timeframe
    }) {
        return Err(StorageError::InvalidData(
            "checkpoint and candle page keys do not match".to_owned(),
        ));
    }
    Ok(())
}

fn row_to_candle(row: sqlx::postgres::PgRow) -> Result<Candle, StorageError> {
    let symbol = Symbol::new(row.try_get::<String, _>("symbol")?)
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
    let timeframe = row
        .try_get::<String, _>("interval")?
        .parse::<Timeframe>()
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
    Candle::new(
        symbol,
        timeframe,
        row.try_get("open_time")?,
        row.try_get("close_time")?,
        row.try_get("open")?,
        row.try_get("high")?,
        row.try_get("low")?,
        row.try_get("close")?,
    )
    .map_err(|error| StorageError::InvalidData(error.to_string()))
}
