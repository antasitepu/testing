#![forbid(unsafe_code)]

mod monitoring_repository;
mod scalping_audit_repository;
mod tracker_repository;
mod virtual_trade_repository;

use execution_service::ExecutionError;
use sqlx::migrate::Migrator;

pub use monitoring_repository::MonitoringRepository;
pub use scalping_audit_repository::ScalpingAuditRepository;
pub use tracker_repository::TrackerRepository;
pub use virtual_trade_repository::VirtualTradeRepository;

pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

fn storage_error(operation: &'static str, error: impl std::fmt::Display) -> ExecutionError {
    ExecutionError::Storage {
        operation,
        message: error.to_string(),
    }
}

fn storage_sqlx_error(operation: &'static str, error: sqlx::Error) -> ExecutionError {
    if matches!(error, sqlx::Error::PoolTimedOut) {
        runtime_observability::record_db_acquire_timeout();
    }
    storage_error(operation, error)
}

#[cfg(test)]
mod observability_tests {
    use super::*;

    #[test]
    fn sqlx_pool_timeout_records_observability_without_changing_error_text() {
        let before = runtime_observability::snapshot().db_acquire_timeout_count;
        let error = storage_sqlx_error("load trades", sqlx::Error::PoolTimedOut);

        assert_eq!(
            error.to_string(),
            "execution storage error during load trades: pool timed out while waiting for an open connection"
        );
        assert_eq!(
            runtime_observability::snapshot().db_acquire_timeout_count,
            before + 1
        );
    }
}
