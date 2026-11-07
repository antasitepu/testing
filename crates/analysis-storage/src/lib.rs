#![forbid(unsafe_code)]

mod backtest_repository;
mod configuration_repository;
mod signal_repository;
mod work_repository;

pub use analysis_service::AnalysisServiceError;
pub use backtest_repository::BacktestRepository;
pub use configuration_repository::ConfigurationRepository;
pub use signal_repository::SignalRepository;
use sqlx::migrate::Migrator;
pub use work_repository::WorkRepository;

pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

fn storage_error(operation: &'static str, error: impl std::fmt::Display) -> AnalysisServiceError {
    AnalysisServiceError::Storage {
        operation,
        message: error.to_string(),
    }
}

fn storage_sqlx_error(operation: &'static str, error: sqlx::Error) -> AnalysisServiceError {
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
        let error = storage_sqlx_error("load profiles", sqlx::Error::PoolTimedOut);

        assert_eq!(
            error.to_string(),
            "storage operation load profiles failed: pool timed out while waiting for an open connection"
        );
        assert_eq!(
            runtime_observability::snapshot().db_acquire_timeout_count,
            before + 1
        );
    }
}
