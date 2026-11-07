use crate::{
    AnalysisCandleSource, AnalysisServiceError, AnalysisWorkStore, ConfigurationStore,
    EvaluationCommit, GapStatusSource, MonitoringEvent, PlanFeedbackStore, PlanRegistry,
    PlanRuntimeStateRecord, RejectedPlanCandidate, SnapshotAvailability,
};
use analysis_domain::{
    EvaluationIdentity, PlanKind, PlanProfileKey, Readiness, ReadinessReason, ReleaseMode,
    StructuredAnalysisError, WorkSource,
};
use analysis_plans::{PlanRuntimeState, apply_terminal_event};
use market_domain::Timeframe;
use runtime_observability::{ActivityKind, record_activity};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

pub struct AnalysisWorker {
    pub work: Arc<dyn AnalysisWorkStore>,
    pub candles: Arc<dyn AnalysisCandleSource>,
    pub gaps: Arc<dyn GapStatusSource>,
    pub configurations: Arc<dyn ConfigurationStore>,
    pub feedback: Arc<dyn PlanFeedbackStore>,
    pub plans: PlanRegistry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerStep {
    Idle,
    Deferred,
    Completed,
}

impl AnalysisWorker {
    pub async fn run_once(
        &self,
        owner: &str,
        now_ms: i64,
    ) -> Result<WorkerStep, AnalysisServiceError> {
        let Some(work) = self.work.lease_next(owner, now_ms).await? else {
            return Ok(WorkerStep::Idle);
        };
        let mode = self.configurations.release_mode(work.release_id).await?;
        let mut profiles = self
            .configurations
            .profiles_for_release(work.release_id, &work.symbol)
            .await?;
        profiles
            .retain(|profile| profile.enabled && profile.key.target_timeframe == work.timeframe);
        profiles.sort_by_key(|profile| profile.key.plan);

        let unresolved_gap = self
            .gaps
            .has_unresolved_gap(&work.symbol, work.timeframe, work.canonical_close_time)
            .await?;
        let mut commit = EvaluationCommit {
            work_id: work.work_id,
            lease_attempt: work.attempt_count,
            ..EvaluationCommit::default()
        };

        for profile in profiles {
            let availability = self.candles.snapshot(&work, &profile).await;
            let mut snapshot = match availability {
                Ok(SnapshotAvailability::Ready(snapshot)) => snapshot,
                Ok(SnapshotAvailability::Waiting) => {
                    self.work.defer(work.work_id, work.attempt_count).await?;
                    return Ok(WorkerStep::Deferred);
                }
                Ok(SnapshotAvailability::Reconcile) => {
                    self.gaps
                        .request_reconciliation(
                            &work.symbol,
                            work.timeframe,
                            work.canonical_close_time,
                        )
                        .await?;
                    self.work.defer(work.work_id, work.attempt_count).await?;
                    return Ok(WorkerStep::Deferred);
                }
                Err(error) => {
                    commit.monitoring_events.push(plan_error_event(
                        work.work_id,
                        &profile,
                        "SNAPSHOT_LOAD_FAILED",
                        &error.to_string(),
                    ));
                    continue;
                }
            };
            if unresolved_gap {
                snapshot.readiness = Readiness::NotReady {
                    reason: ReadinessReason::UnresolvedGap,
                };
            }
            let mut state = self
                .work
                .runtime_state(&profile.key, work.release_id)
                .await?
                .map(|record| {
                    serde_json::from_value(record.state)
                        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))
                })
                .transpose()?
                .unwrap_or_else(|| PlanRuntimeState::neutral(&profile));
            if let Some(signal) = state.active_signal.clone() {
                for event in self
                    .feedback
                    .terminal_signal_events(signal.signal_id)
                    .await?
                {
                    apply_terminal_event(&mut state, &event)
                        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
                    if state.active_signal.is_none() {
                        break;
                    }
                }
            }
            state.clear_terminal_setup();
            let identity = EvaluationIdentity {
                symbol: profile.key.symbol.clone(),
                plan: profile.key.plan,
                target_timeframe: profile.key.target_timeframe,
                release_id: work.release_id,
                algorithm_version: profile.versions.algorithm_version.clone(),
                parameter_version: profile.versions.parameter_version.clone(),
                evaluated_closed_candle_time: work.canonical_close_time,
            };
            commit.evaluations.push(identity.clone());
            if suppresses_rsi_historical_evaluation(profile.key.plan, work.source) {
                commit.runtime_states.push(PlanRuntimeStateRecord {
                    key: profile.key.clone(),
                    release_id: work.release_id,
                    state: serde_json::to_value(&state)
                        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
                    last_evaluated_close_time: work.canonical_close_time,
                });
                let code = "RSI_NON_LIVE_EVALUATION_SUPPRESSED";
                commit.monitoring_events.push(MonitoringEvent {
                    event_id: deterministic_event_id(work.work_id, profile.key.plan.as_str(), code),
                    event_type: code.to_owned(),
                    entity_type: "ANALYSIS_WORK".to_owned(),
                    entity_id: Some(work.work_id),
                    payload: json!({
                        "key": profile.key,
                        "workSource": work.source.as_str(),
                        "releaseId": work.release_id,
                        "evaluatedCloseTime": work.canonical_close_time,
                        "reason": code,
                    }),
                });
                continue;
            }
            if suppresses_new_rsi_m1_entry(&profile.key, &state) {
                commit.runtime_states.push(PlanRuntimeStateRecord {
                    key: profile.key.clone(),
                    release_id: work.release_id,
                    state: serde_json::to_value(&state)
                        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
                    last_evaluated_close_time: work.canonical_close_time,
                });
                let code = "RSI_M1_NEW_ENTRY_SUPPRESSED";
                commit.monitoring_events.push(MonitoringEvent {
                    event_id: deterministic_event_id(work.work_id, profile.key.plan.as_str(), code),
                    event_type: code.to_owned(),
                    entity_type: "ANALYSIS_WORK".to_owned(),
                    entity_id: Some(work.work_id),
                    payload: json!({
                        "key": profile.key,
                        "workSource": work.source.as_str(),
                        "releaseId": work.release_id,
                        "evaluatedCloseTime": work.canonical_close_time,
                        "reason": code,
                    }),
                });
                continue;
            }
            let feedback = if let Some(signal) = &state.active_signal {
                self.feedback.pending_feedback(signal.signal_id).await?
            } else {
                Vec::new()
            };
            let Some(evaluator) = self.plans.get(profile.key.plan) else {
                tracing::warn!(
                    work_id = %work.work_id,
                    symbol = %profile.key.symbol,
                    plan = profile.key.plan.as_str(),
                    timeframe = profile.key.target_timeframe.as_str(),
                    release_id = %work.release_id,
                    evaluated_close_time = work.canonical_close_time,
                    latency_ms = 0_u128,
                    cache_hit = false,
                    deduplicated = false,
                    error_code = "PLAN_NOT_REGISTERED",
                    "analysis evaluation failed"
                );
                commit.monitoring_events.push(plan_error_event(
                    work.work_id,
                    &profile,
                    "PLAN_NOT_REGISTERED",
                    "plan evaluator is not registered",
                ));
                continue;
            };
            let started_at = Instant::now();
            match evaluator.evaluate(&profile, &snapshot, &state, &feedback) {
                Ok(evaluation) => {
                    tracing::debug!(
                        work_id = %work.work_id,
                        evaluation_identity = ?identity,
                        symbol = %profile.key.symbol,
                        plan = profile.key.plan.as_str(),
                        timeframe = profile.key.target_timeframe.as_str(),
                        release_id = %work.release_id,
                        evaluated_close_time = work.canonical_close_time,
                        latency_ms = started_at.elapsed().as_millis(),
                        cache_hit = false,
                        deduplicated = false,
                        error_code = "",
                        "analysis evaluation completed"
                    );
                    commit.runtime_states.push(PlanRuntimeStateRecord {
                        key: profile.key.clone(),
                        release_id: work.release_id,
                        state: serde_json::to_value(&evaluation.next_state).map_err(|error| {
                            AnalysisServiceError::InvalidData(error.to_string())
                        })?,
                        last_evaluated_close_time: work.canonical_close_time,
                    });
                    commit.consumed_feedback.extend(feedback);
                    for rejected in evaluation.rejected_candidates {
                        commit.rejected_candidates.push(RejectedPlanCandidate {
                            candidate_id: rejected.candidate_id,
                            key: profile.key.clone(),
                            reason: rejected.reason,
                            payload: rejected.payload,
                            evaluated_at: work.canonical_close_time,
                        });
                    }
                    if mode == ReleaseMode::PaperActive {
                        if let Some(signal) = evaluation.signal {
                            commit.signals.push(signal);
                        }
                        commit.signal_events.extend(evaluation.events);
                        commit.directives.extend(evaluation.directives);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        work_id = %work.work_id,
                        evaluation_identity = ?identity,
                        symbol = %profile.key.symbol,
                        plan = profile.key.plan.as_str(),
                        timeframe = profile.key.target_timeframe.as_str(),
                        release_id = %work.release_id,
                        evaluated_close_time = work.canonical_close_time,
                        latency_ms = started_at.elapsed().as_millis(),
                        cache_hit = false,
                        deduplicated = false,
                        error_code = "PLAN_EVALUATION_FAILED",
                        "analysis evaluation failed"
                    );
                    commit.monitoring_events.push(plan_error_event(
                        work.work_id,
                        &profile,
                        "PLAN_EVALUATION_FAILED",
                        &error.to_string(),
                    ));
                }
            }
        }
        self.work.complete_evaluation(commit).await?;
        record_activity(ActivityKind::Analysis, work.canonical_close_time);
        Ok(WorkerStep::Completed)
    }

    pub async fn run(
        &self,
        owner: String,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), AnalysisServiceError> {
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.run_once(&owner, crate::scheduler::now_ms()).await {
                Ok(WorkerStep::Completed) => {}
                Ok(WorkerStep::Idle | WorkerStep::Deferred) => {
                    tokio::select! {
                        _ = shutdown.changed() => {},
                        _ = sleep(Duration::from_millis(100)) => {},
                    }
                }
                Err(error) => {
                    tracing::error!(
                        owner = %owner,
                        error = %error,
                        "analysis worker iteration failed; retrying"
                    );
                    tokio::select! {
                        _ = shutdown.changed() => {},
                        _ = sleep(Duration::from_millis(500)) => {},
                    }
                }
            }
        }
    }
}

fn suppresses_rsi_historical_evaluation(plan: PlanKind, source: WorkSource) -> bool {
    plan == PlanKind::Rsi && source != WorkSource::Live
}

/// Shared live entry policy for the worker and historical live-arrival replay.
pub fn suppresses_new_rsi_m1_entry(key: &PlanProfileKey, state: &PlanRuntimeState) -> bool {
    key.plan == PlanKind::Rsi
        && key.target_timeframe == Timeframe::M1
        && state.active_signal.is_none()
}

fn plan_error_event(
    work_id: Uuid,
    profile: &analysis_domain::MaterializedProfile,
    code: &str,
    message: &str,
) -> MonitoringEvent {
    let error = StructuredAnalysisError {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable: false,
    };
    MonitoringEvent {
        event_id: deterministic_event_id(work_id, profile.key.plan.as_str(), code),
        event_type: "PLAN_ERROR".to_owned(),
        entity_type: "ANALYSIS_WORK".to_owned(),
        entity_id: Some(work_id),
        payload: json!({
            "key": profile.key,
            "error": error,
        }),
    }
}

fn deterministic_event_id(work_id: Uuid, plan: &str, code: &str) -> Uuid {
    let mut value = work_id.as_u128();
    for byte in plan.bytes().chain(code.bytes()) {
        value = value
            .wrapping_mul(0x100000001b3)
            .wrapping_add(u128::from(byte));
    }
    Uuid::from_u128(value)
}
