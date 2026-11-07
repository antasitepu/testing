use market_domain::{Candle, MarketSession, SessionStatus};
use market_service::{
    CandleSessionClass, CommoditySessionGenerator, ServiceError, SessionRecord, SessionSource,
    TradableCandlePolicy, canonicalize_sessions,
};
use market_storage::SessionRepository;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHistoryConfig {
    pub overrides_path: PathBuf,
    pub policy_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOverrides {
    pub version: String,
    pub records: Vec<SessionRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCoverageReport {
    pub start_time: i64,
    pub end_time: i64,
    pub regular_ms: i64,
    pub no_trading_ms: i64,
    pub unknown_ms: i64,
    pub conflicts: usize,
    pub policy_version: String,
    pub override_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlatRun {
    pub start_time: i64,
    pub end_time: i64,
    pub duration_ms: i64,
    pub price: Decimal,
}

#[derive(Clone)]
pub struct HistoricalSessionCoordinator {
    repository: SessionRepository,
    config: SessionHistoryConfig,
    overrides: SessionOverrides,
}

impl HistoricalSessionCoordinator {
    pub fn new(
        repository: SessionRepository,
        config: SessionHistoryConfig,
        overrides: SessionOverrides,
    ) -> Self {
        Self {
            repository,
            config,
            overrides,
        }
    }

    pub async fn ensure_coverage(
        &self,
        session_market: &str,
        start_time: i64,
        end_time: i64,
        source_event_time: i64,
        persist: bool,
    ) -> Result<SessionCoverageReport, ServiceError> {
        let mut records =
            CommoditySessionGenerator.generate(start_time, end_time, source_event_time)?;
        let existing = self
            .repository
            .canonical_between(session_market, start_time, end_time)
            .await
            .map_err(|error| ServiceError::Storage {
                operation: "load historical market sessions",
                message: error.to_string(),
            })?;
        records.extend(
            existing
                .into_iter()
                .filter(|record| record.source == SessionSource::BinanceApi),
        );
        records.extend(self.overrides.records.iter().cloned());
        let canonical = canonicalize_sessions(start_time, end_time, &records)?;
        let mut regular_ms = 0_i64;
        let mut no_trading_ms = 0_i64;
        for record in &canonical {
            let duration = record.session.end_time - record.session.start_time;
            match record.session.status {
                SessionStatus::Regular => regular_ms += duration,
                SessionStatus::NoTrading => no_trading_ms += duration,
            }
        }
        let covered_ms = regular_ms + no_trading_ms;
        let unknown_ms = (end_time - start_time).saturating_sub(covered_ms);
        if persist {
            if unknown_ms != 0 {
                return Err(ServiceError::InvalidData(format!(
                    "historical session coverage has {unknown_ms} unknown milliseconds"
                )));
            }
            self.repository
                .replace_canonical_range(session_market, start_time, end_time, &canonical)
                .await
                .map_err(|error| ServiceError::Storage {
                    operation: "persist canonical historical sessions",
                    message: error.to_string(),
                })?;
        }
        Ok(SessionCoverageReport {
            start_time,
            end_time,
            regular_ms,
            no_trading_ms,
            unknown_ms,
            conflicts: 0,
            policy_version: self.config.policy_version.clone(),
            override_version: self.overrides.version.clone(),
        })
    }
}

pub fn load_session_overrides(
    config: &SessionHistoryConfig,
    expected_market: &str,
) -> Result<SessionOverrides, ServiceError> {
    let fallback = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(&config.overrides_path);
    let resolved = if config.overrides_path.is_absolute() || config.overrides_path.exists() {
        config.overrides_path.clone()
    } else {
        fallback
    };
    let content = std::fs::read_to_string(&resolved).map_err(|error| {
        ServiceError::InvalidData(format!(
            "read session overrides {}: {error}",
            resolved.display()
        ))
    })?;
    parse_session_overrides(&content, expected_market)
}

pub fn parse_session_overrides(
    input: &str,
    expected_market: &str,
) -> Result<SessionOverrides, ServiceError> {
    let raw: RawOverrideFile = serde_json::from_str(input)
        .map_err(|error| ServiceError::InvalidData(format!("parse session overrides: {error}")))?;
    let version = raw.version.trim();
    if version.is_empty() {
        return Err(ServiceError::InvalidData(
            "session override version must not be blank".to_owned(),
        ));
    }
    let mut references = HashSet::new();
    let mut records = Vec::with_capacity(raw.overrides.len());
    for item in raw.overrides {
        if item.session_market != expected_market {
            return Err(ServiceError::InvalidData(format!(
                "override market {} does not match {expected_market}",
                item.session_market
            )));
        }
        if item.end_time <= item.start_time {
            return Err(ServiceError::InvalidData(
                "session override end must be later than start".to_owned(),
            ));
        }
        if item.reference.trim().is_empty() || item.reason.trim().is_empty() {
            return Err(ServiceError::InvalidData(
                "session override reference and reason must not be blank".to_owned(),
            ));
        }
        if !references.insert(item.reference.clone()) {
            return Err(ServiceError::InvalidData(format!(
                "duplicate session override reference {}",
                item.reference
            )));
        }
        let status = match item.status.as_str() {
            "REGULAR" => SessionStatus::Regular,
            "NO_TRADING" => SessionStatus::NoTrading,
            other => {
                return Err(ServiceError::InvalidData(format!(
                    "unsupported session override status {other}"
                )));
            }
        };
        records.push(SessionRecord {
            session: MarketSession::new(
                item.session_market,
                status,
                item.start_time,
                item.end_time,
                item.end_time,
            )
            .map_err(|error| ServiceError::InvalidData(error.to_string()))?,
            source: SessionSource::ManualOverride,
            source_reference: Some(item.reference),
        });
    }
    records.sort_by_key(|record| (record.session.start_time, record.session.end_time));
    if records
        .windows(2)
        .any(|window| window[0].session.end_time > window[1].session.start_time)
    {
        return Err(ServiceError::InvalidData(
            "session overrides overlap".to_owned(),
        ));
    }
    Ok(SessionOverrides {
        version: version.to_owned(),
        records,
    })
}

pub fn detect_regular_flat_runs(
    candles: &[Candle],
    sessions: &[MarketSession],
    minimum_duration_ms: i64,
) -> Vec<FlatRun> {
    let policy = TradableCandlePolicy;
    let mut completed = Vec::new();
    let mut active: Option<FlatRun> = None;
    for candle in candles {
        let is_flat =
            candle.open == candle.high && candle.open == candle.low && candle.open == candle.close;
        let tradable = policy.classify(candle, sessions) == CandleSessionClass::Tradable;
        if !is_flat || !tradable {
            finish_flat_run(&mut completed, active.take(), minimum_duration_ms);
            continue;
        }
        let Some(end_time) = candle.close_time.checked_add(1) else {
            finish_flat_run(&mut completed, active.take(), minimum_duration_ms);
            continue;
        };
        match active.as_mut() {
            Some(run) if run.end_time == candle.open_time && run.price == candle.close => {
                run.end_time = end_time;
                run.duration_ms = run.end_time - run.start_time;
            }
            Some(_) => {
                finish_flat_run(&mut completed, active.take(), minimum_duration_ms);
                active = Some(FlatRun {
                    start_time: candle.open_time,
                    end_time,
                    duration_ms: end_time - candle.open_time,
                    price: candle.close,
                });
            }
            None => {
                active = Some(FlatRun {
                    start_time: candle.open_time,
                    end_time,
                    duration_ms: end_time - candle.open_time,
                    price: candle.close,
                });
            }
        }
    }
    finish_flat_run(&mut completed, active, minimum_duration_ms);
    completed
}

fn finish_flat_run(completed: &mut Vec<FlatRun>, run: Option<FlatRun>, minimum_duration_ms: i64) {
    if let Some(run) = run
        && run.duration_ms >= minimum_duration_ms
    {
        completed.push(run);
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOverrideFile {
    version: String,
    overrides: Vec<RawOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOverride {
    session_market: String,
    status: String,
    start_time: i64,
    end_time: i64,
    reference: String,
    reason: String,
}

#[cfg(test)]
mod database_audit_tests {
    use super::*;
    use crate::tuning_dataset::{TuningDatasetLoader, dataset_audit_line};
    use market_domain::{Candle, Symbol, Timeframe};
    use market_storage::MIGRATOR;
    use sqlx::{Row, postgres::PgPoolOptions};

    #[tokio::test]
    #[ignore = "manual session-history audit; requires DATABASE_URL"]
    async fn print_session_history_audit_from_database() {
        dotenvy::dotenv().ok();
        let pool = production_pool().await;
        let symbol = Symbol::new("XAUUSDT").unwrap();
        let start: i64 = sqlx::query_scalar("SELECT MIN(open_time) FROM candles WHERE symbol = $1")
            .bind(symbol.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        let (end, source_event_time): (i64, i64) = sqlx::query_as(
            "SELECT MAX(session_end_time), MAX(source_event_time) \
             FROM market_sessions WHERE session_market = 'COMMODITY'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let config = SessionHistoryConfig {
            overrides_path: workspace_override_path(),
            policy_version: "commodity-session-policy-v1".to_owned(),
        };
        let overrides = load_session_overrides(&config, "COMMODITY").unwrap();
        let persist = std::env::var("GOLD_SESSION_PERSIST").as_deref() == Ok("1");
        let report = HistoricalSessionCoordinator::new(
            SessionRepository::new(pool.clone()),
            config,
            overrides,
        )
        .ensure_coverage("COMMODITY", start, end, source_event_time, persist)
        .await
        .unwrap();
        println!(
            "SESSION,{},{},{},{},{},{},{},{}",
            report.start_time,
            report.end_time,
            report.regular_ms,
            report.no_trading_ms,
            report.unknown_ms,
            report.conflicts,
            report.policy_version,
            report.override_version
        );
        if persist {
            let sessions = SessionRepository::new(pool.clone())
                .between("COMMODITY", start, end)
                .await
                .unwrap();
            let candles = load_m1_candles(&pool, &symbol, start, end).await;
            for run in detect_regular_flat_runs(&candles, &sessions, 30 * 60_000) {
                println!(
                    "FLAT_RUN,{},{},{},{}",
                    run.start_time, run.end_time, run.duration_ms, run.price
                );
            }
        }
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "manual session-clean dataset audit; requires DATABASE_URL"]
    async fn print_session_clean_dataset_audit_from_database() {
        dotenvy::dotenv().ok();
        let pool = production_pool().await;
        let symbol = Symbol::new("XAUUSDT").unwrap();
        let config = SessionHistoryConfig {
            overrides_path: workspace_override_path(),
            policy_version: "commodity-session-policy-v1".to_owned(),
        };
        let overrides = load_session_overrides(&config, "COMMODITY").unwrap();
        let loader =
            TuningDatasetLoader::new(pool.clone(), config.policy_version, overrides.version);
        for timeframe in Timeframe::ALL {
            let dataset = loader
                .load(&symbol, timeframe, None, 500_000)
                .await
                .unwrap();
            println!("{}", dataset_audit_line(&dataset.audit));
        }
        pool.close().await;
    }

    async fn production_pool() -> sqlx::PgPool {
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn workspace_override_path() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("config/commodity-session-overrides.json")
    }

    async fn load_m1_candles(
        pool: &sqlx::PgPool,
        symbol: &Symbol,
        start: i64,
        end: i64,
    ) -> Vec<Candle> {
        sqlx::query(
            "SELECT open_time, close_time, open, high, low, close \
             FROM candles WHERE symbol = $1 AND interval = '1m' \
               AND open_time >= $2 AND close_time < $3 ORDER BY open_time",
        )
        .bind(symbol.as_str())
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            Candle::new(
                symbol.clone(),
                Timeframe::M1,
                row.get("open_time"),
                row.get("close_time"),
                row.get("open"),
                row.get("high"),
                row.get("low"),
                row.get("close"),
            )
            .unwrap()
        })
        .collect()
    }
}
