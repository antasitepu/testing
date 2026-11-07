use crate::{StorageError, parse_session_status, session_status_text};
use market_domain::MarketSession;
use market_service::{SessionRecord, SessionSource, canonicalize_sessions};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct SessionRepository {
    pool: PgPool,
}

impl SessionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert_sessions(&self, sessions: &[MarketSession]) -> Result<(), StorageError> {
        let mut by_market = BTreeMap::<String, Vec<&MarketSession>>::new();
        for session in sessions {
            by_market
                .entry(session.session_market.clone())
                .or_default()
                .push(session);
        }
        let mut transaction = self.pool.begin().await?;
        for (session_market, incoming) in by_market {
            let incoming_start = incoming
                .iter()
                .map(|session| session.start_time)
                .min()
                .unwrap();
            let incoming_end = incoming
                .iter()
                .map(|session| session.end_time)
                .max()
                .unwrap();
            let existing = sqlx::query(
                "SELECT session_market, session_status, session_start_time, session_end_time, \
                        source_event_time, source_kind, source_reference \
                 FROM market_sessions \
                 WHERE session_market = $1 \
                   AND session_end_time > $2 \
                   AND session_start_time < $3 \
                 ORDER BY session_start_time, session_end_time, source_event_time \
                 FOR UPDATE",
            )
            .bind(&session_market)
            .bind(incoming_start)
            .bind(incoming_end)
            .fetch_all(&mut *transaction)
            .await?
            .into_iter()
            .map(row_to_session_record)
            .collect::<Result<Vec<_>, _>>()?;
            let range_start = existing
                .iter()
                .map(|record| record.session.start_time)
                .chain(std::iter::once(incoming_start))
                .min()
                .unwrap();
            let range_end = existing
                .iter()
                .map(|record| record.session.end_time)
                .chain(std::iter::once(incoming_end))
                .max()
                .unwrap();
            let mut candidates = existing.clone();
            candidates.extend(incoming.into_iter().map(|session| SessionRecord {
                session: session.clone(),
                source: SessionSource::BinanceApi,
                source_reference: None,
            }));
            let canonical = canonicalize_sessions(range_start, range_end, &candidates)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?;
            if canonical == existing {
                continue;
            }
            sqlx::query(
                "DELETE FROM market_sessions \
                 WHERE session_market = $1 \
                   AND session_end_time > $2 \
                   AND session_start_time < $3",
            )
            .bind(&session_market)
            .bind(range_start)
            .bind(range_end)
            .execute(&mut *transaction)
            .await?;
            for record in canonical {
                insert_session_record(&mut transaction, &record).await?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn replace_canonical_range(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
        sessions: &[SessionRecord],
    ) -> Result<(), StorageError> {
        validate_canonical_range(session_market, start, end, sessions)?;
        let mut transaction = self.pool.begin().await?;
        let existing = sqlx::query(
            "SELECT session_market, session_status, session_start_time, session_end_time, \
                    source_event_time, source_kind, source_reference \
             FROM market_sessions \
             WHERE session_market = $1 \
               AND session_end_time > $2 \
               AND session_start_time < $3 \
             ORDER BY session_start_time, session_end_time, source_event_time \
             FOR UPDATE",
        )
        .bind(session_market)
        .bind(start)
        .bind(end)
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(row_to_session_record)
        .collect::<Result<Vec<_>, _>>()?;
        let range_start = existing
            .iter()
            .map(|record| record.session.start_time)
            .chain(std::iter::once(start))
            .min()
            .unwrap();
        let range_end = existing
            .iter()
            .map(|record| record.session.end_time)
            .chain(std::iter::once(end))
            .max()
            .unwrap();
        let mut candidates = sessions.to_vec();
        for record in &existing {
            if record.session.start_time < start {
                candidates.push(session_fragment(record, record.session.start_time, start)?);
            }
            if record.session.end_time > end {
                candidates.push(session_fragment(record, end, record.session.end_time)?);
            }
        }
        let canonical = canonicalize_sessions(range_start, range_end, &candidates)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        validate_canonical_range(session_market, range_start, range_end, &canonical)?;
        if canonical == existing {
            transaction.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "DELETE FROM market_sessions \
             WHERE session_market = $1 \
               AND session_end_time > $2 \
               AND session_start_time < $3",
        )
        .bind(session_market)
        .bind(range_start)
        .bind(range_end)
        .execute(&mut *transaction)
        .await?;
        for record in canonical {
            insert_session_record(&mut transaction, &record).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn canonical_between(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<SessionRecord>, StorageError> {
        if end <= start {
            return Err(StorageError::InvalidData(
                "session range end must be later than start".to_owned(),
            ));
        }
        sqlx::query(
            "SELECT session_market, session_status, session_start_time, session_end_time, \
                    source_event_time, source_kind, source_reference \
             FROM market_sessions \
             WHERE session_market = $1 \
               AND session_end_time > $2 \
               AND session_start_time < $3 \
             ORDER BY session_start_time, session_end_time, source_event_time",
        )
        .bind(session_market)
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_session_record)
        .collect()
    }

    pub async fn covering(
        &self,
        session_market: &str,
        timestamp: i64,
    ) -> Result<Option<MarketSession>, StorageError> {
        let row = sqlx::query(
            r#"
            SELECT session_market, session_status, session_start_time,
                   session_end_time, source_event_time
            FROM market_sessions
            WHERE session_market = $1
              AND session_start_time <= $2
              AND session_end_time > $2
            ORDER BY source_event_time DESC
            LIMIT 1
            "#,
        )
        .bind(session_market)
        .bind(timestamp)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_session).transpose()
    }

    pub async fn between(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<MarketSession>, StorageError> {
        if end <= start {
            return Err(StorageError::InvalidData(
                "session range end must be later than start".to_owned(),
            ));
        }
        sqlx::query(
            r#"
            SELECT session_market, session_status, session_start_time,
                   session_end_time, source_event_time
            FROM market_sessions
            WHERE session_market = $1
              AND session_end_time > $2
              AND session_start_time < $3
            ORDER BY session_start_time ASC, source_event_time ASC
            "#,
        )
        .bind(session_market)
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_session)
        .collect()
    }
}

async fn insert_session_record(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &SessionRecord,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO market_sessions \
         (session_market, session_status, session_start_time, session_end_time, \
          source_event_time, source_kind, source_reference) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&record.session.session_market)
    .bind(session_status_text(record.session.status))
    .bind(record.session.start_time)
    .bind(record.session.end_time)
    .bind(record.session.source_event_time)
    .bind(session_source_text(record.source))
    .bind(&record.source_reference)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn session_fragment(
    record: &SessionRecord,
    start_time: i64,
    end_time: i64,
) -> Result<SessionRecord, StorageError> {
    Ok(SessionRecord {
        session: MarketSession::new(
            record.session.session_market.clone(),
            record.session.status,
            start_time,
            end_time,
            record.session.source_event_time,
        )
        .map_err(|error| StorageError::InvalidData(error.to_string()))?,
        source: record.source,
        source_reference: record.source_reference.clone(),
    })
}

fn validate_canonical_range(
    session_market: &str,
    start: i64,
    end: i64,
    sessions: &[SessionRecord],
) -> Result<(), StorageError> {
    if session_market.is_empty() || end <= start || sessions.is_empty() {
        return Err(StorageError::InvalidData(
            "canonical session range is empty or invalid".to_owned(),
        ));
    }
    if sessions[0].session.start_time != start
        || sessions
            .last()
            .is_none_or(|record| record.session.end_time != end)
    {
        return Err(StorageError::InvalidData(
            "canonical sessions do not cover the requested boundaries".to_owned(),
        ));
    }
    for (index, record) in sessions.iter().enumerate() {
        if record.session.session_market != session_market {
            return Err(StorageError::InvalidData(
                "canonical session market does not match requested market".to_owned(),
            ));
        }
        if index > 0 {
            let previous = &sessions[index - 1];
            if previous.session.end_time != record.session.start_time {
                return Err(StorageError::InvalidData(
                    "canonical sessions contain a gap or overlap".to_owned(),
                ));
            }
            if previous.session.status == record.session.status
                && previous.source == record.source
                && previous.source_reference == record.source_reference
            {
                return Err(StorageError::InvalidData(
                    "adjacent canonical sessions with identical metadata must be merged".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn row_to_session(row: sqlx::postgres::PgRow) -> Result<MarketSession, StorageError> {
    MarketSession::new(
        row.try_get::<String, _>("session_market")?,
        parse_session_status(row.try_get("session_status")?)?,
        row.try_get("session_start_time")?,
        row.try_get("session_end_time")?,
        row.try_get("source_event_time")?,
    )
    .map_err(|error| StorageError::InvalidData(error.to_string()))
}

fn row_to_session_record(row: sqlx::postgres::PgRow) -> Result<SessionRecord, StorageError> {
    let source: String = row.try_get("source_kind")?;
    let source_reference = row.try_get("source_reference")?;
    Ok(SessionRecord {
        session: row_to_session(row)?,
        source: parse_session_source(&source)?,
        source_reference,
    })
}

fn session_source_text(source: SessionSource) -> &'static str {
    match source {
        SessionSource::BinanceApi => "BINANCE_API",
        SessionSource::DerivedWeekly => "DERIVED_WEEKLY",
        SessionSource::ManualOverride => "MANUAL_OVERRIDE",
    }
}

fn parse_session_source(source: &str) -> Result<SessionSource, StorageError> {
    match source {
        "BINANCE_API" => Ok(SessionSource::BinanceApi),
        "DERIVED_WEEKLY" => Ok(SessionSource::DerivedWeekly),
        "MANUAL_OVERRIDE" => Ok(SessionSource::ManualOverride),
        _ => Err(StorageError::InvalidData(format!(
            "unknown market session source {source}"
        ))),
    }
}
