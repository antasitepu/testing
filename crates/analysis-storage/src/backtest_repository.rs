use crate::{storage_error, storage_sqlx_error};
use analysis_domain::StructuredAnalysisError;
use analysis_service::{
    AnalysisServiceError, BacktestRunInput, BacktestRunRecord, BacktestRunResult,
    BacktestRunStatus, BacktestRunView, BacktestStore, BacktestTradeRecord,
};
use async_trait::async_trait;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct BacktestRepository {
    pool: PgPool,
}

impl BacktestRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BacktestStore for BacktestRepository {
    async fn create_run(&self, input: BacktestRunRecord) -> Result<Uuid, AnalysisServiceError> {
        sqlx::query(
            "INSERT INTO backtest_runs (run_id, status, input, code_commit_hash) \
             VALUES ($1, 'PENDING', $2, $3)",
        )
        .bind(input.run_id)
        .bind(input.input_json)
        .bind(input.code_commit_hash)
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("create backtest run", error))?;
        Ok(input.run_id)
    }

    async fn complete_run(&self, result: BacktestRunResult) -> Result<(), AnalysisServiceError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin backtest completion", error))?;
        let update = sqlx::query(
            "UPDATE backtest_runs \
             SET status = 'COMPLETED', metrics = $2, error = NULL, completed_at = NOW() \
             WHERE run_id = $1 AND status IN ('PENDING', 'RUNNING')",
        )
        .bind(result.run_id)
        .bind(result.metrics)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("complete backtest run", error))?;
        if update.rows_affected() != 1 {
            return Err(AnalysisServiceError::Conflict(
                "backtest run is not completable".to_owned(),
            ));
        }
        for trade in result.trades {
            sqlx::query(
                "INSERT INTO backtest_trades (trade_id, run_id, sequence, trade) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(trade.trade_id)
            .bind(result.run_id)
            .bind(trade.sequence)
            .bind(trade.trade)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert backtest trade", error))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit backtest completion", error))
    }

    async fn fail_run(
        &self,
        run_id: Uuid,
        error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin backtest failure", error))?;
        sqlx::query("DELETE FROM backtest_trades WHERE run_id = $1")
            .bind(run_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("clear failed backtest trades", error))?;
        let update = sqlx::query(
            "UPDATE backtest_runs \
             SET status = 'FAILED', metrics = NULL, error = $2, completed_at = NOW() \
             WHERE run_id = $1 AND status IN ('PENDING', 'RUNNING')",
        )
        .bind(run_id)
        .bind(
            serde_json::to_value(error)
                .map_err(|error| storage_error("serialize backtest failure", error))?,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("fail backtest run", error))?;
        if update.rows_affected() != 1 {
            return Err(AnalysisServiceError::Conflict(
                "backtest run is not fail-able".to_owned(),
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit backtest failure", error))
    }

    async fn run(&self, run_id: Uuid) -> Result<Option<BacktestRunView>, AnalysisServiceError> {
        let Some(row) = sqlx::query(
            "SELECT status, input, metrics, error FROM backtest_runs WHERE run_id = $1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("read backtest run", error))?
        else {
            return Ok(None);
        };
        let status = parse_status(
            row.try_get("status")
                .map_err(|error| storage_error("decode backtest status", error))?,
        )?;
        let input_json = row
            .try_get("input")
            .map_err(|error| storage_error("decode backtest input", error))?;
        let input: BacktestRunInput = serde_json::from_value(input_json)
            .map_err(|error| storage_error("deserialize backtest input", error))?;
        let trades = sqlx::query(
            "SELECT trade_id, sequence, trade FROM backtest_trades \
             WHERE run_id = $1 ORDER BY sequence ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("read backtest trades", error))?
        .into_iter()
        .map(|row| {
            Ok(BacktestTradeRecord {
                trade_id: row.try_get("trade_id")?,
                sequence: row.try_get("sequence")?,
                trade: row.try_get("trade")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| storage_error("decode backtest trade", error))?;
        Ok(Some(BacktestRunView {
            run_id,
            status,
            input,
            metrics: row
                .try_get("metrics")
                .map_err(|error| storage_error("decode backtest metrics", error))?,
            error: row
                .try_get("error")
                .map_err(|error| storage_error("decode backtest error", error))?,
            trades,
        }))
    }
}

fn parse_status(value: String) -> Result<BacktestRunStatus, AnalysisServiceError> {
    match value.as_str() {
        "PENDING" => Ok(BacktestRunStatus::Pending),
        "RUNNING" => Ok(BacktestRunStatus::Running),
        "COMPLETED" => Ok(BacktestRunStatus::Completed),
        "FAILED" => Ok(BacktestRunStatus::Failed),
        _ => Err(AnalysisServiceError::InvalidData(format!(
            "unknown backtest status: {value}"
        ))),
    }
}
