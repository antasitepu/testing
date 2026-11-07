use market_domain::{Candle, Symbol, Timeframe};
use market_service::{CandleSessionClass, ServiceError, TradableCandlePolicy};
use market_storage::SessionRepository;
use sqlx::{PgPool, Row};
use std::fmt::Write;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatasetAudit {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub first_open_time: Option<i64>,
    pub last_close_time: Option<i64>,
    pub tradable_count: usize,
    pub excluded_count: usize,
    pub unknown_count: usize,
    pub policy_version: String,
    pub override_version: String,
    pub dataset_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuningDataset {
    pub candles: Vec<Candle>,
    pub audit: DatasetAudit,
}

#[derive(Clone)]
pub struct TuningDatasetLoader {
    pool: PgPool,
    policy_version: String,
    override_version: String,
}

pub fn dataset_audit_line(audit: &DatasetAudit) -> String {
    format!(
        "DATASET,{},{},{},{},{},{},{},{},{},{}",
        audit.symbol,
        audit.timeframe.as_str(),
        audit
            .first_open_time
            .map_or_else(String::new, |value| value.to_string()),
        audit
            .last_close_time
            .map_or_else(String::new, |value| value.to_string()),
        audit.tradable_count,
        audit.excluded_count,
        audit.unknown_count,
        audit.policy_version,
        audit.override_version,
        audit.dataset_hash
    )
}

impl TuningDatasetLoader {
    pub fn new(
        pool: PgPool,
        policy_version: impl Into<String>,
        override_version: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            policy_version: policy_version.into(),
            override_version: override_version.into(),
        }
    }

    pub async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        before: Option<i64>,
        max_bars: usize,
    ) -> Result<TuningDataset, ServiceError> {
        if max_bars == 0 {
            return Err(ServiceError::InvalidData(
                "tuning dataset max bars must be greater than zero".to_owned(),
            ));
        }
        let session_market: String =
            sqlx::query_scalar("SELECT session_market FROM markets WHERE symbol = $1")
                .bind(symbol.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| storage_error("load tuning market", error))?
                .ok_or_else(|| ServiceError::InvalidData(format!("unknown market {symbol}")))?;
        let rows = sqlx::query(
            "SELECT open_time, close_time, open, high, low, close \
             FROM candles \
             WHERE symbol = $1 AND interval = $2 \
               AND ($3::BIGINT IS NULL OR open_time < $3) \
             ORDER BY open_time",
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(before)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_error("load raw tuning candles", error))?;
        let mut raw = Vec::with_capacity(rows.len());
        for row in rows {
            raw.push(
                Candle::new(
                    symbol.clone(),
                    timeframe,
                    row.get("open_time"),
                    row.get("close_time"),
                    row.get("open"),
                    row.get("high"),
                    row.get("low"),
                    row.get("close"),
                )
                .map_err(|error| ServiceError::InvalidData(error.to_string()))?,
            );
        }
        let sessions = match (raw.first(), raw.last()) {
            (Some(first), Some(last)) => SessionRepository::new(self.pool.clone())
                .between(
                    &session_market,
                    first.open_time,
                    last.close_time.saturating_add(1),
                )
                .await
                .map_err(|error| storage_error("load tuning market sessions", error))?,
            _ => Vec::new(),
        };
        let policy = TradableCandlePolicy;
        let mut tradable = Vec::new();
        let mut excluded_count = 0_usize;
        let mut unknown_count = 0_usize;
        for candle in raw {
            match policy.classify(&candle, &sessions) {
                CandleSessionClass::Tradable => tradable.push(candle),
                CandleSessionClass::NoTrading => excluded_count += 1,
                CandleSessionClass::Unknown => unknown_count += 1,
            }
        }
        if unknown_count > 0 {
            return Err(ServiceError::InvalidData(format!(
                "unknown session coverage: {unknown_count} candle{}",
                if unknown_count == 1 { "" } else { "s" }
            )));
        }
        if tradable.len() > max_bars {
            tradable.drain(0..tradable.len() - max_bars);
        }
        let dataset_hash = dataset_hash(&tradable, &self.policy_version, &self.override_version);
        let audit = DatasetAudit {
            symbol: symbol.as_str().to_owned(),
            timeframe,
            first_open_time: tradable.first().map(|candle| candle.open_time),
            last_close_time: tradable.last().map(|candle| candle.close_time),
            tradable_count: tradable.len(),
            excluded_count,
            unknown_count,
            policy_version: self.policy_version.clone(),
            override_version: self.override_version.clone(),
            dataset_hash,
        };
        Ok(TuningDataset {
            candles: tradable,
            audit,
        })
    }
}

fn dataset_hash(candles: &[Candle], policy_version: &str, override_version: &str) -> String {
    let mut encoded = String::new();
    let _ = writeln!(encoded, "dataset-v1|{policy_version}|{override_version}");
    for candle in candles {
        let _ = writeln!(
            encoded,
            "{}|{}|{}|{}|{}|{}",
            candle.open_time,
            candle.close_time,
            candle.open.normalize(),
            candle.high.normalize(),
            candle.low.normalize(),
            candle.close.normalize()
        );
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in encoded.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn storage_error(operation: &'static str, error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage {
        operation,
        message: error.to_string(),
    }
}
