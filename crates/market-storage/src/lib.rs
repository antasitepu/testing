#![forbid(unsafe_code)]

mod candle_repository;
mod market_repository;
mod service_ports;
mod session_repository;
mod sync_repository;

use market_domain::{MarketStatus, SessionStatus, SyncRunStatus};
use sqlx::migrate::Migrator;

pub use candle_repository::CandleRepository;
pub use market_repository::MarketRepository;
pub use session_repository::SessionRepository;
pub use sync_repository::SyncRepository;

pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("postgres query failed: {0}")]
    Sqlx(sqlx::Error),
    #[error("stored market data is invalid: {0}")]
    InvalidData(String),
}

impl From<sqlx::Error> for StorageError {
    fn from(error: sqlx::Error) -> Self {
        if matches!(error, sqlx::Error::PoolTimedOut) {
            runtime_observability::record_db_acquire_timeout();
        }
        Self::Sqlx(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageCheckpoint {
    pub symbol: market_domain::Symbol,
    pub timeframe: market_domain::Timeframe,
    pub oldest_synced_time: i64,
    pub latest_synced_time: i64,
    pub operation: StorageSyncOperation,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageSyncOperation {
    Recent,
    Backfill,
}

impl StorageSyncOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Backfill => "backfill",
        }
    }
}

fn parse_market_status(value: String) -> Result<MarketStatus, StorageError> {
    match value.as_str() {
        "TRADING" => Ok(MarketStatus::Trading),
        "HALTED" => Ok(MarketStatus::Halted),
        "UNKNOWN" => Ok(MarketStatus::Unknown),
        _ => Err(StorageError::InvalidData(format!(
            "unknown market status {value}"
        ))),
    }
}

fn parse_session_status(value: String) -> Result<SessionStatus, StorageError> {
    match value.as_str() {
        "REGULAR" => Ok(SessionStatus::Regular),
        "NO_TRADING" => Ok(SessionStatus::NoTrading),
        _ => Err(StorageError::InvalidData(format!(
            "unknown session status {value}"
        ))),
    }
}

fn session_status_text(value: SessionStatus) -> &'static str {
    match value {
        SessionStatus::Regular => "REGULAR",
        SessionStatus::NoTrading => "NO_TRADING",
    }
}

fn parse_sync_status(value: String) -> Result<SyncRunStatus, StorageError> {
    match value.as_str() {
        "pending" => Ok(SyncRunStatus::Pending),
        "running" => Ok(SyncRunStatus::Running),
        "complete" => Ok(SyncRunStatus::Complete),
        "failed" => Ok(SyncRunStatus::Failed),
        _ => Err(StorageError::InvalidData(format!(
            "unknown sync status {value}"
        ))),
    }
}

fn sync_status_text(value: SyncRunStatus) -> &'static str {
    match value {
        SyncRunStatus::Pending => "pending",
        SyncRunStatus::Running => "running",
        SyncRunStatus::Complete => "complete",
        SyncRunStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod observability_tests {
    use super::*;

    #[test]
    fn pool_timeout_conversion_records_observability_without_changing_error_variant() {
        let before = runtime_observability::snapshot().db_acquire_timeout_count;
        let error = StorageError::from(sqlx::Error::PoolTimedOut);

        assert!(matches!(
            error,
            StorageError::Sqlx(sqlx::Error::PoolTimedOut)
        ));
        assert_eq!(
            runtime_observability::snapshot().db_acquire_timeout_count,
            before + 1
        );
    }
}
