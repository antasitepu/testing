use crate::{configuration_repository::parse_plan, storage_error, storage_sqlx_error};
use analysis_domain::{
    AnalysisWorkItem, PlanFeedback, PlanProfileKey, StructuredAnalysisError, WorkSource, WorkStatus,
};
use analysis_service::{
    AnalysisServiceError, AnalysisWorkStore, EvaluationCommit, PlanRuntimeStateRecord,
    RejectedCandidateStore, RejectedPlanCandidate, RejectedPlanCandidateQuery,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use market_domain::{Symbol, Timeframe};
use sqlx::{PgPool, Row};
use std::str::FromStr;
use uuid::Uuid;

const DEFAULT_LEASE_DURATION_MS: i64 = 60_000;

#[derive(Clone)]
pub struct WorkRepository {
    pool: PgPool,
    lease_duration_ms: i64,
}

impl WorkRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            lease_duration_ms: DEFAULT_LEASE_DURATION_MS,
        }
    }

    pub fn with_lease_duration_ms(mut self, lease_duration_ms: i64) -> Self {
        self.lease_duration_ms = lease_duration_ms;
        self
    }

    pub async fn enqueue(&self, item: &AnalysisWorkItem) -> Result<(), AnalysisServiceError> {
        item.validate()
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        sqlx::query(
            "INSERT INTO analysis_work_items \
             (work_id, symbol, interval, close_time, release_id, source, status, \
              attempt_count, last_error, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'PENDING', 0, $7, to_timestamp($8 / 1000.0)) \
             ON CONFLICT (symbol, interval, close_time, release_id) DO NOTHING",
        )
        .bind(item.work_id)
        .bind(item.symbol.as_str())
        .bind(item.timeframe.as_str())
        .bind(item.canonical_close_time)
        .bind(item.release_id)
        .bind(item.source.as_str())
        .bind(
            item.error
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| storage_error("serialize work error", error))?,
        )
        .bind(item.created_at)
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("enqueue analysis work", error))?;
        Ok(())
    }
}

#[async_trait]
impl AnalysisWorkStore for WorkRepository {
    async fn lease_next(
        &self,
        owner: &str,
        now_ms: i64,
    ) -> Result<Option<AnalysisWorkItem>, AnalysisServiceError> {
        if owner.trim().is_empty() || now_ms < 0 {
            return Err(AnalysisServiceError::InvalidData(
                "lease owner must be set and time non-negative".to_owned(),
            ));
        }
        let row = sqlx::query(
            "WITH candidate AS ( \
                 SELECT work_id FROM analysis_work_items \
                 WHERE status = 'PENDING' \
                    OR (status = 'LEASED' AND lease_expires_at <= to_timestamp($2 / 1000.0)) \
                 ORDER BY close_time, created_at, work_id \
                 FOR UPDATE SKIP LOCKED \
                 LIMIT 1 \
             ) \
             UPDATE analysis_work_items w \
             SET status = 'LEASED', lease_owner = $1, \
                 lease_expires_at = to_timestamp(($2 + $3) / 1000.0), \
                 attempt_count = attempt_count + 1 \
             FROM candidate \
             WHERE w.work_id = candidate.work_id \
             RETURNING w.*",
        )
        .bind(owner)
        .bind(now_ms)
        .bind(self.lease_duration_ms)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("lease analysis work", error))?;
        row.map(row_to_work).transpose()
    }

    async fn complete_evaluation(
        &self,
        commit: EvaluationCommit,
    ) -> Result<(), AnalysisServiceError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin evaluation commit", error))?;

        for evaluation in &commit.evaluations {
            evaluation
                .validate()
                .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
            sqlx::query(
                "INSERT INTO analysis_evaluations \
                 (symbol, plan, interval, release_id, algorithm_version, \
                  parameter_version, evaluated_closed_candle_time) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(evaluation.symbol.as_str())
            .bind(evaluation.plan.as_str())
            .bind(evaluation.target_timeframe.as_str())
            .bind(evaluation.release_id)
            .bind(&evaluation.algorithm_version)
            .bind(&evaluation.parameter_version)
            .bind(evaluation.evaluated_closed_candle_time)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert analysis evaluation", error))?;
        }

        for state in &commit.runtime_states {
            sqlx::query(
                "INSERT INTO plan_runtime_states \
                 (symbol, plan, interval, release_id, state, last_evaluated_close_time) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (symbol, plan, interval) DO UPDATE SET \
                    release_id = EXCLUDED.release_id, state = EXCLUDED.state, \
                    last_evaluated_close_time = EXCLUDED.last_evaluated_close_time, \
                    updated_at = NOW()",
            )
            .bind(state.key.symbol.as_str())
            .bind(state.key.plan.as_str())
            .bind(state.key.target_timeframe.as_str())
            .bind(state.release_id)
            .bind(&state.state)
            .bind(state.last_evaluated_close_time)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("upsert plan runtime state", error))?;
        }

        for signal in &commit.signals {
            let definition = serde_json::to_value(signal)
                .map_err(|error| storage_error("serialize signal definition", error))?;
            let evidence = serde_json::to_value(&signal.evidence)
                .map_err(|error| storage_error("serialize signal evidence", error))?;
            sqlx::query(
                "INSERT INTO signals \
                 (signal_id, symbol, plan, interval, definition, evidence, release_id, \
                  algorithm_version, parameter_version, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                         to_timestamp($10 / 1000.0))",
            )
            .bind(signal.signal_id)
            .bind(signal.symbol.as_str())
            .bind(signal.plan.as_str())
            .bind(signal.target_timeframe.as_str())
            .bind(definition)
            .bind(evidence)
            .bind(signal.release_id)
            .bind(&signal.algorithm_version)
            .bind(&signal.parameter_version)
            .bind(signal.created_at)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert signal", error))?;
        }

        for event in &commit.signal_events {
            sqlx::query(
                "INSERT INTO signal_events \
                 (event_id, signal_id, sequence, event_type, payload, effective_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (signal_id, sequence) DO NOTHING",
            )
            .bind(event.event_id)
            .bind(event.signal_id)
            .bind(event.sequence)
            .bind(event.kind.as_str())
            .bind(
                serde_json::to_value(event)
                    .map_err(|error| storage_error("serialize signal event", error))?,
            )
            .bind(event.effective_at)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert signal event", error))?;
        }

        for directive in &commit.directives {
            sqlx::query(
                "INSERT INTO trade_management_directives \
                 (directive_id, signal_id, virtual_trade_id, kind, payload, effective_at, \
                  source_closed_candle_time, algorithm_version, parameter_version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(directive.directive_id)
            .bind(directive.signal_id)
            .bind(directive.virtual_trade_id)
            .bind(directive.kind.as_str())
            .bind(
                serde_json::to_value(directive)
                    .map_err(|error| storage_error("serialize management directive", error))?,
            )
            .bind(directive.effective_at)
            .bind(directive.source_closed_candle_time)
            .bind(&directive.algorithm_version)
            .bind(&directive.parameter_version)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert management directive", error))?;
        }

        for feedback in &commit.consumed_feedback {
            consume_plan_feedback(&mut transaction, feedback).await?;
        }

        for candidate in &commit.rejected_candidates {
            sqlx::query(
                "INSERT INTO rejected_plan_candidates \
                 (candidate_id, symbol, plan, interval, reason, payload, evaluated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(candidate.candidate_id)
            .bind(candidate.key.symbol.as_str())
            .bind(candidate.key.plan.as_str())
            .bind(candidate.key.target_timeframe.as_str())
            .bind(&candidate.reason)
            .bind(&candidate.payload)
            .bind(candidate.evaluated_at)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert rejected candidate", error))?;
        }

        for event in &commit.monitoring_events {
            sqlx::query(
                "INSERT INTO monitoring_events \
                 (event_id, event_type, entity_type, entity_id, payload) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(event.event_id)
            .bind(&event.event_type)
            .bind(&event.entity_type)
            .bind(event.entity_id)
            .bind(&event.payload)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert monitoring event", error))?;
        }

        if commit.inject_failure_after_writes {
            return Err(AnalysisServiceError::Storage {
                operation: "evaluation commit failure injection",
                message: "injected rollback".to_owned(),
            });
        }

        let result = sqlx::query(
            "UPDATE analysis_work_items \
             SET status = 'COMPLETED', completed_at = NOW(), \
                 lease_owner = NULL, lease_expires_at = NULL \
             WHERE work_id = $1 AND status = 'LEASED' AND attempt_count = $2",
        )
        .bind(commit.work_id)
        .bind(i64::from(commit.lease_attempt))
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("complete analysis work", error))?;
        if result.rows_affected() != 1 {
            return Err(AnalysisServiceError::Conflict(
                "work item is not leased".to_owned(),
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit evaluation", error))
    }

    async fn runtime_state(
        &self,
        key: &analysis_domain::PlanProfileKey,
        release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError> {
        let row = sqlx::query(
            "SELECT state, last_evaluated_close_time FROM plan_runtime_states \
             WHERE symbol = $1 AND plan = $2 AND interval = $3 AND release_id = $4",
        )
        .bind(key.symbol.as_str())
        .bind(key.plan.as_str())
        .bind(key.target_timeframe.as_str())
        .bind(release_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load plan runtime state", error))?;
        Ok(row.map(|row| PlanRuntimeStateRecord {
            key: key.clone(),
            release_id,
            state: row.get("state"),
            last_evaluated_close_time: row.get("last_evaluated_close_time"),
        }))
    }

    async fn defer(&self, work_id: Uuid, lease_attempt: u32) -> Result<(), AnalysisServiceError> {
        let result = sqlx::query(
            "UPDATE analysis_work_items SET status = 'PENDING', lease_owner = NULL, \
             lease_expires_at = NULL \
             WHERE work_id = $1 AND status = 'LEASED' AND attempt_count = $2",
        )
        .bind(work_id)
        .bind(i64::from(lease_attempt))
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("defer analysis work", error))?;
        if result.rows_affected() != 1 {
            return Err(AnalysisServiceError::Conflict(
                "work item is not leased".to_owned(),
            ));
        }
        Ok(())
    }

    async fn fail(
        &self,
        work_id: Uuid,
        lease_attempt: u32,
        error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        let payload = serde_json::to_value(error)
            .map_err(|error| storage_error("serialize analysis failure", error))?;
        let result = sqlx::query(
            "UPDATE analysis_work_items \
             SET status = 'FAILED', last_error = $2, completed_at = NOW(), \
                 lease_owner = NULL, lease_expires_at = NULL \
             WHERE work_id = $1 AND status = 'LEASED' AND attempt_count = $3",
        )
        .bind(work_id)
        .bind(payload)
        .bind(i64::from(lease_attempt))
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("fail analysis work", error))?;
        if result.rows_affected() != 1 {
            return Err(AnalysisServiceError::Conflict(
                "work item is not leased".to_owned(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RejectedCandidateStore for WorkRepository {
    async fn recent_rejections(
        &self,
        query: RejectedPlanCandidateQuery,
    ) -> Result<Vec<RejectedPlanCandidate>, AnalysisServiceError> {
        if query.limit == 0 || query.limit > 500 {
            return Err(AnalysisServiceError::InvalidData(
                "rejected candidate limit must be 1..=500".to_owned(),
            ));
        }
        if query.timeframes.is_empty() {
            return Err(AnalysisServiceError::InvalidData(
                "rejected candidate timeframes must not be empty".to_owned(),
            ));
        }
        let intervals = query
            .timeframes
            .iter()
            .map(|timeframe| timeframe.as_str().to_owned())
            .collect::<Vec<_>>();
        let rows = sqlx::query(
            "SELECT candidate_id, symbol, plan, interval, reason, payload, evaluated_at \
             FROM rejected_plan_candidates \
             WHERE symbol = $1 AND interval = ANY($2::TEXT[]) \
             ORDER BY evaluated_at DESC, candidate_id DESC LIMIT $3",
        )
        .bind(query.symbol.as_str())
        .bind(&intervals)
        .bind(i64::from(query.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load rejected candidates", error))?;
        rows.into_iter().map(row_to_rejected_candidate).collect()
    }
}

fn row_to_work(row: sqlx::postgres::PgRow) -> Result<AnalysisWorkItem, AnalysisServiceError> {
    let symbol: String = row.get("symbol");
    let interval: String = row.get("interval");
    let source: String = row.get("source");
    let status: String = row.get("status");
    let error: Option<serde_json::Value> = row.get("last_error");
    Ok(AnalysisWorkItem {
        work_id: row.get("work_id"),
        symbol: Symbol::new(symbol)
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        timeframe: Timeframe::from_str(&interval)
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        canonical_close_time: row.get("close_time"),
        release_id: row.get("release_id"),
        source: parse_source(&source)?,
        status: parse_status(&status)?,
        lease_owner: row.get("lease_owner"),
        lease_expires_at: row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .map(|value| value.timestamp_millis()),
        attempt_count: u32::try_from(row.get::<i32, _>("attempt_count"))
            .map_err(|_| AnalysisServiceError::InvalidData("negative attempt count".to_owned()))?,
        error: error
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        created_at: row.get::<DateTime<Utc>, _>("created_at").timestamp_millis(),
        completed_at: row
            .get::<Option<DateTime<Utc>>, _>("completed_at")
            .map(|value| value.timestamp_millis()),
    })
}

fn row_to_rejected_candidate(
    row: sqlx::postgres::PgRow,
) -> Result<RejectedPlanCandidate, AnalysisServiceError> {
    let symbol: String = row.get("symbol");
    let plan: String = row.get("plan");
    let interval: String = row.get("interval");
    Ok(RejectedPlanCandidate {
        candidate_id: row.get("candidate_id"),
        key: PlanProfileKey {
            symbol: Symbol::new(symbol)
                .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
            plan: parse_plan(&plan)?,
            target_timeframe: Timeframe::from_str(&interval)
                .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        },
        reason: row.get("reason"),
        payload: row.get("payload"),
        evaluated_at: row.get("evaluated_at"),
    })
}

async fn consume_plan_feedback(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    feedback: &PlanFeedback,
) -> Result<(), AnalysisServiceError> {
    let (signal_id, virtual_trade_id, feedback_type) = feedback_identity(feedback);
    sqlx::query(
        "UPDATE plan_feedback SET consumed_at = NOW() \
         WHERE signal_id = $1 AND virtual_trade_id = $2 AND feedback_type = $3 \
           AND consumed_at IS NULL",
    )
    .bind(signal_id)
    .bind(virtual_trade_id)
    .bind(feedback_type)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("consume plan feedback", error))?;
    Ok(())
}

fn feedback_identity(feedback: &PlanFeedback) -> (Uuid, Uuid, &'static str) {
    match feedback {
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "ACTUAL_ENTRY"),
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "PARTIAL_TARGET_FILLED"),
        PlanFeedback::TradeClosed {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "TRADE_CLOSED"),
    }
}

fn parse_source(value: &str) -> Result<WorkSource, AnalysisServiceError> {
    match value {
        "LIVE" => Ok(WorkSource::Live),
        "CATCH_UP" => Ok(WorkSource::CatchUp),
        "DOWNTIME_RECONCILED" => Ok(WorkSource::DowntimeReconciled),
        _ => Err(AnalysisServiceError::InvalidData(format!(
            "unknown work source {value}"
        ))),
    }
}

fn parse_status(value: &str) -> Result<WorkStatus, AnalysisServiceError> {
    match value {
        "PENDING" => Ok(WorkStatus::Pending),
        "LEASED" => Ok(WorkStatus::Leased),
        "COMPLETED" => Ok(WorkStatus::Completed),
        "FAILED" => Ok(WorkStatus::Failed),
        _ => Err(AnalysisServiceError::InvalidData(format!(
            "unknown work status {value}"
        ))),
    }
}
