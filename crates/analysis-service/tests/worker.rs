use analysis_domain::{
    AnalysisWorkItem, CanonicalMarketSnapshot, Direction, ExecutionInstruction,
    MaterializedProfile, PlanFeedback, PlanKind, PlanParameters, PlanProfileKey, Readiness,
    ReadinessReason, ReleaseMode, RsiPlanParameters, SetupState, SignalDefinition,
    SignalExecutionPlan, SmcParameters, TargetLeg, VersionIdentity, WorkSource, WorkStatus,
};
use analysis_plans::{
    PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, PositionStage, apply_feedback,
};
use analysis_service::{
    AnalysisCandleSource, AnalysisServiceError, AnalysisWorkStore, AnalysisWorker,
    ConfigurationRelease, ConfigurationStore, EvaluationCommit, GapStatusSource, PlanFeedbackStore,
    PlanRegistry, PlanRuntimeStateRecord, ReleaseLifecycle, SnapshotAvailability, ValidationGate,
    WorkerStep,
};
use async_trait::async_trait;
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn work_with_source(release_id: Uuid, source: WorkSource) -> AnalysisWorkItem {
    work_with_timeframe(release_id, source, Timeframe::M1)
}

fn work_with_timeframe(
    release_id: Uuid,
    source: WorkSource,
    timeframe: Timeframe,
) -> AnalysisWorkItem {
    AnalysisWorkItem {
        work_id: Uuid::now_v7(),
        symbol: symbol(),
        timeframe,
        canonical_close_time: 239_999,
        release_id,
        source,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: 1,
        completed_at: None,
    }
}

fn work(release_id: Uuid) -> AnalysisWorkItem {
    work_with_source(release_id, WorkSource::Live)
}

fn profile(release_id: Uuid, plan: PlanKind, enabled: bool) -> MaterializedProfile {
    profile_with_timeframe(release_id, plan, enabled, Timeframe::M1)
}

fn profile_with_timeframe(
    release_id: Uuid,
    plan: PlanKind,
    enabled: bool,
    target_timeframe: Timeframe,
) -> MaterializedProfile {
    let parameters = match plan {
        PlanKind::Rsi => PlanParameters::Rsi(RsiPlanParameters {
            rsi_period: 14,
            oversold_threshold: Decimal::new(30, 0),
            overbought_threshold: Decimal::new(70, 0),
            divergence_required: false,
            minimum_pivot_distance: 2,
            maximum_pivot_distance: 20,
            confirmed_swing_lookback: 10,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            target_rr: Decimal::TWO,
            max_bars_valid: 5,
            reversal_confirmation_bars: 2,
        }),
        _ => PlanParameters::Smc(SmcParameters {
            higher_timeframe: None,
            htf_mode: analysis_domain::HtfFilterMode::Off,
            weighted_countertrend_penalty: Decimal::ZERO,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            max_bars_valid: 5,
        }),
    };
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan,
            target_timeframe,
        },
        enabled,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(
            release_id,
            format!("{}-v1", plan.as_str()),
            format!("{}-{}-v1", plan.as_str(), target_timeframe.as_str()),
        )
        .unwrap(),
        parameters,
        required_timeframes: Vec::new(),
        minimum_history_bars: 1,
        warmup_bars: 1,
    }
}

fn snapshot(readiness: Readiness) -> CanonicalMarketSnapshot {
    CanonicalMarketSnapshot::empty(
        symbol(),
        Timeframe::M1,
        Decimal::new(1, 1),
        239_999,
        readiness,
    )
    .unwrap()
}

#[derive(Default)]
struct WorkMock {
    queue: Mutex<VecDeque<AnalysisWorkItem>>,
    commits: Mutex<Vec<EvaluationCommit>>,
    deferred: Mutex<Vec<Uuid>>,
    runtime_state: Mutex<Option<PlanRuntimeStateRecord>>,
    complete_failures_remaining: Mutex<usize>,
}

#[async_trait]
impl AnalysisWorkStore for WorkMock {
    async fn lease_next(
        &self,
        owner: &str,
        now_ms: i64,
    ) -> Result<Option<AnalysisWorkItem>, AnalysisServiceError> {
        let mut item = self.queue.lock().unwrap().pop_front();
        if let Some(item) = &mut item {
            item.status = WorkStatus::Leased;
            item.lease_owner = Some(owner.to_owned());
            item.lease_expires_at = Some(now_ms + 60_000);
        }
        Ok(item)
    }

    async fn complete_evaluation(
        &self,
        commit: EvaluationCommit,
    ) -> Result<(), AnalysisServiceError> {
        let mut failures = self.complete_failures_remaining.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(AnalysisServiceError::Storage {
                operation: "complete evaluation",
                message: "injected transient failure".to_owned(),
            });
        }
        drop(failures);
        self.commits.lock().unwrap().push(commit);
        Ok(())
    }

    async fn runtime_state(
        &self,
        key: &PlanProfileKey,
        release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError> {
        Ok(self
            .runtime_state
            .lock()
            .unwrap()
            .clone()
            .filter(|record| &record.key == key && record.release_id == release_id))
    }

    async fn defer(&self, work_id: Uuid, _lease_attempt: u32) -> Result<(), AnalysisServiceError> {
        self.deferred.lock().unwrap().push(work_id);
        Ok(())
    }

    async fn fail(
        &self,
        _work_id: Uuid,
        _lease_attempt: u32,
        _error: analysis_domain::StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }
}

#[derive(Default)]
struct FeedbackMock {
    pending: Mutex<Vec<PlanFeedback>>,
    pending_requests: Mutex<Vec<Uuid>>,
    signal_events: Mutex<Vec<analysis_domain::SignalEvent>>,
}

#[async_trait]
impl PlanFeedbackStore for FeedbackMock {
    async fn pending_feedback(
        &self,
        signal_id: Uuid,
    ) -> Result<Vec<PlanFeedback>, AnalysisServiceError> {
        self.pending_requests.lock().unwrap().push(signal_id);
        Ok(self.pending.lock().unwrap().clone())
    }

    async fn terminal_signal_events(
        &self,
        _signal_id: Uuid,
    ) -> Result<Vec<analysis_domain::SignalEvent>, AnalysisServiceError> {
        Ok(self.signal_events.lock().unwrap().clone())
    }
}

struct ConfigMock {
    mode: ReleaseMode,
    profiles: Vec<MaterializedProfile>,
}

#[async_trait]
impl ConfigurationStore for ConfigMock {
    async fn create_draft(
        &self,
        _release: &ConfigurationRelease,
    ) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn activate(
        &self,
        _release_id: Uuid,
        _gate: &ValidationGate,
    ) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn active_profiles(
        &self,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }

    async fn release_mode(&self, _release_id: Uuid) -> Result<ReleaseMode, AnalysisServiceError> {
        Ok(self.mode)
    }

    async fn profiles_for_release(
        &self,
        _release_id: Uuid,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }
}

struct CandleMock {
    values: Mutex<HashMap<PlanKind, Result<SnapshotAvailability, AnalysisServiceError>>>,
}

#[async_trait]
impl AnalysisCandleSource for CandleMock {
    async fn snapshot(
        &self,
        _work: &AnalysisWorkItem,
        profile: &MaterializedProfile,
    ) -> Result<SnapshotAvailability, AnalysisServiceError> {
        self.values
            .lock()
            .unwrap()
            .remove(&profile.key.plan)
            .unwrap_or_else(|| Ok(SnapshotAvailability::Ready(snapshot(Readiness::Ready))))
    }
}

#[derive(Default)]
struct GapMock {
    unresolved: bool,
    reconciled: Mutex<Vec<(Timeframe, i64)>>,
}

#[async_trait]
impl GapStatusSource for GapMock {
    async fn has_unresolved_gap(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _close_time: i64,
    ) -> Result<bool, AnalysisServiceError> {
        Ok(self.unresolved)
    }

    async fn request_reconciliation(
        &self,
        _symbol: &Symbol,
        timeframe: Timeframe,
        close_time: i64,
    ) -> Result<(), AnalysisServiceError> {
        self.reconciled
            .lock()
            .unwrap()
            .push((timeframe, close_time));
        Ok(())
    }
}

struct StubEvaluator {
    kind: PlanKind,
    fail: bool,
    order: Arc<Mutex<Vec<PlanKind>>>,
    readiness: Arc<Mutex<Vec<Readiness>>>,
    feedback_seen: Arc<Mutex<Vec<Vec<PlanFeedback>>>>,
}

impl PlanEvaluator for StubEvaluator {
    fn kind(&self) -> PlanKind {
        self.kind
    }

    fn algorithm_version(&self) -> &'static str {
        "stub-v1"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        self.order.lock().unwrap().push(self.kind);
        self.readiness.lock().unwrap().push(snapshot.readiness);
        self.feedback_seen.lock().unwrap().push(feedback.to_vec());
        if self.fail {
            return Err(PlanError::Serialization("injected".to_owned()));
        }
        let mut next_state = state.clone();
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        Ok(PlanEvaluation {
            next_state,
            signal: Some(stub_signal(profile)),
            events: Vec::new(),
            directives: Vec::new(),
            rejected_candidates: Vec::new(),
        })
    }
}

fn stub_signal(profile: &MaterializedProfile) -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: profile.key.symbol.clone(),
        plan: profile.key.plan,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: 239_999,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(Decimal::new(4_100, 0)),
        stop_loss: Decimal::new(4_090, 0),
        primary_target: Decimal::new(4_120, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        effective_at: 240_000,
        detected_at: 240_000,
        published_at: 240_000,
        release_id: profile.versions.release_id,
        algorithm_version: profile.versions.algorithm_version.clone(),
        parameter_version: profile.versions.parameter_version.clone(),
        created_at: 240_000,
        expires_at: None,
        htf_context: None,
    }
}

fn worker(
    release_id: Uuid,
    mode: ReleaseMode,
    profiles: Vec<MaterializedProfile>,
    candles: HashMap<PlanKind, Result<SnapshotAvailability, AnalysisServiceError>>,
    gap: GapMock,
    evaluators: Vec<Arc<dyn PlanEvaluator>>,
) -> (AnalysisWorker, Arc<WorkMock>, Arc<GapMock>) {
    let work_store = Arc::new(WorkMock::default());
    let feedback = Arc::new(FeedbackMock::default());
    work_store.queue.lock().unwrap().push_back(work(release_id));
    let gap = Arc::new(gap);
    (
        AnalysisWorker {
            work: work_store.clone(),
            candles: Arc::new(CandleMock {
                values: Mutex::new(candles),
            }),
            gaps: gap.clone(),
            configurations: Arc::new(ConfigMock { mode, profiles }),
            feedback,
            plans: PlanRegistry::new(evaluators),
        },
        work_store,
        gap,
    )
}

#[tokio::test]
async fn stable_order_disabled_research_and_unresolved_gap_are_atomic() {
    let release_id = Uuid::now_v7();
    let order = Arc::new(Mutex::new(Vec::new()));
    let readiness = Arc::new(Mutex::new(Vec::new()));
    let profiles = vec![
        profile(release_id, PlanKind::Smc, true),
        profile(release_id, PlanKind::Rsi, false),
        profile(release_id, PlanKind::Ict, true),
    ];
    let evaluators = [PlanKind::Ict, PlanKind::Smc]
        .into_iter()
        .map(|kind| {
            Arc::new(StubEvaluator {
                kind,
                fail: false,
                order: order.clone(),
                readiness: readiness.clone(),
                feedback_seen: Arc::new(Mutex::new(Vec::new())),
            }) as Arc<dyn PlanEvaluator>
        })
        .collect();
    let (worker, work, _) = worker(
        release_id,
        ReleaseMode::Research,
        profiles,
        HashMap::new(),
        GapMock {
            unresolved: true,
            ..GapMock::default()
        },
        evaluators,
    );
    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert_eq!(*order.lock().unwrap(), vec![PlanKind::Ict, PlanKind::Smc]);
    assert!(readiness.lock().unwrap().iter().all(|value| {
        *value
            == Readiness::NotReady {
                reason: ReadinessReason::UnresolvedGap,
            }
    }));
    let commits = work.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].evaluations.len(), 2);
    assert_eq!(commits[0].runtime_states.len(), 2);
    assert!(commits[0].signals.is_empty());
}

#[tokio::test]
async fn barrier_wait_and_reconcile_defer_without_partial_commit() {
    for availability in [
        SnapshotAvailability::Waiting,
        SnapshotAvailability::Reconcile,
    ] {
        let release_id = Uuid::now_v7();
        let profile = profile(release_id, PlanKind::Ict, true);
        let mut candles = HashMap::new();
        candles.insert(PlanKind::Ict, Ok(availability.clone()));
        let (worker, work, gap) = worker(
            release_id,
            ReleaseMode::PaperActive,
            vec![profile],
            candles,
            GapMock::default(),
            vec![Arc::new(StubEvaluator {
                kind: PlanKind::Ict,
                fail: false,
                order: Arc::new(Mutex::new(Vec::new())),
                readiness: Arc::new(Mutex::new(Vec::new())),
                feedback_seen: Arc::new(Mutex::new(Vec::new())),
            })],
        );
        assert_eq!(
            worker.run_once("worker", 1_000).await.unwrap(),
            WorkerStep::Deferred
        );
        assert!(work.commits.lock().unwrap().is_empty());
        assert_eq!(work.deferred.lock().unwrap().len(), 1);
        assert_eq!(
            gap.reconciled.lock().unwrap().len(),
            usize::from(availability == SnapshotAvailability::Reconcile)
        );
    }
}

#[tokio::test]
async fn plan_and_snapshot_failures_are_isolated_from_other_profiles() {
    let release_id = Uuid::now_v7();
    let order = Arc::new(Mutex::new(Vec::new()));
    let readiness = Arc::new(Mutex::new(Vec::new()));
    let profiles = vec![
        profile(release_id, PlanKind::Ict, true),
        profile(release_id, PlanKind::Smc, true),
        profile(release_id, PlanKind::Rsi, true),
    ];
    let mut candles = HashMap::new();
    candles.insert(
        PlanKind::Rsi,
        Err(AnalysisServiceError::InvalidData(
            "timeframe unavailable".to_owned(),
        )),
    );
    let evaluators = vec![
        Arc::new(StubEvaluator {
            kind: PlanKind::Ict,
            fail: true,
            order: order.clone(),
            readiness: readiness.clone(),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        }) as Arc<dyn PlanEvaluator>,
        Arc::new(StubEvaluator {
            kind: PlanKind::Smc,
            fail: false,
            order,
            readiness,
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        }),
    ];
    let (worker, work, _) = worker(
        release_id,
        ReleaseMode::PaperActive,
        profiles,
        candles,
        GapMock::default(),
        evaluators,
    );
    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    let commits = work.commits.lock().unwrap();
    assert_eq!(commits[0].evaluations.len(), 2);
    assert_eq!(commits[0].runtime_states.len(), 1);
    assert_eq!(commits[0].signals.len(), 1);
    assert_eq!(commits[0].monitoring_events.len(), 2);
}

#[tokio::test]
async fn pending_plan_feedback_is_delivered_to_evaluator_and_consumed_on_commit() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, PlanKind::Ict, true);
    let mut signal = stub_signal(&profile);
    signal.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: signal.stop_loss,
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: analysis_domain::PriceZone::new(Decimal::new(4_118, 0), Decimal::new(4_120, 0))
            .unwrap(),
    });
    let virtual_trade_id = Uuid::now_v7();
    let feedback = vec![
        PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id,
            time: 240_001,
            price: Decimal::new(4_105, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id: signal.signal_id,
            virtual_trade_id,
            time: 240_002,
            price: Decimal::new(4_115, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];
    let mut state = PlanRuntimeState::neutral(&profile);
    state.setup_state = SetupState::EntryAvailable;
    state.active_signal = Some(signal.clone());
    let work_store = Arc::new(WorkMock::default());
    work_store.queue.lock().unwrap().push_back(work(release_id));
    *work_store.runtime_state.lock().unwrap() = Some(PlanRuntimeStateRecord {
        key: profile.key.clone(),
        release_id,
        state: serde_json::to_value(&state).unwrap(),
        last_evaluated_close_time: 200_000,
    });
    let feedback_store = Arc::new(FeedbackMock {
        pending: Mutex::new(feedback.clone()),
        pending_requests: Mutex::new(Vec::new()),
        signal_events: Mutex::new(Vec::new()),
    });
    let feedback_seen = Arc::new(Mutex::new(Vec::new()));
    let worker = AnalysisWorker {
        work: work_store.clone(),
        candles: Arc::new(CandleMock {
            values: Mutex::new(HashMap::new()),
        }),
        gaps: Arc::new(GapMock::default()),
        configurations: Arc::new(ConfigMock {
            mode: ReleaseMode::PaperActive,
            profiles: vec![profile],
        }),
        feedback: feedback_store,
        plans: PlanRegistry::new(vec![Arc::new(StubEvaluator {
            kind: PlanKind::Ict,
            fail: false,
            order: Arc::new(Mutex::new(Vec::new())),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: feedback_seen.clone(),
        }) as Arc<dyn PlanEvaluator>]),
    };

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert_eq!(*feedback_seen.lock().unwrap(), vec![feedback.clone()]);
    assert_eq!(
        work_store.commits.lock().unwrap()[0].consumed_feedback,
        feedback
    );
    let committed: PlanRuntimeState = serde_json::from_value(
        work_store.commits.lock().unwrap()[0].runtime_states[0]
            .state
            .clone(),
    )
    .unwrap();
    assert_eq!(committed.position_stage, PositionStage::Runner);
    assert_eq!(committed.remaining_fraction, Decimal::new(5, 1));
    assert_eq!(committed.realized_r, Decimal::new(5, 1));
}

#[tokio::test]
async fn non_live_rsi_work_skips_evaluator_and_actionable_outputs() {
    for source in [WorkSource::CatchUp, WorkSource::DowntimeReconciled] {
        let release_id = Uuid::now_v7();
        let evaluator_order = Arc::new(Mutex::new(Vec::new()));
        let rsi_profile = profile(release_id, PlanKind::Rsi, true);
        let expected_key = rsi_profile.key.clone();
        let (worker, work_store, _) = worker(
            release_id,
            ReleaseMode::PaperActive,
            vec![rsi_profile],
            HashMap::new(),
            GapMock::default(),
            vec![Arc::new(StubEvaluator {
                kind: PlanKind::Rsi,
                fail: false,
                order: evaluator_order.clone(),
                readiness: Arc::new(Mutex::new(Vec::new())),
                feedback_seen: Arc::new(Mutex::new(Vec::new())),
            })],
        );
        let historical_work = work_with_source(release_id, source);
        let work_id = historical_work.work_id;
        work_store.queue.lock().unwrap().clear();
        work_store.queue.lock().unwrap().push_back(historical_work);

        assert_eq!(
            worker.run_once("worker", 1_000).await.unwrap(),
            WorkerStep::Completed
        );
        assert!(evaluator_order.lock().unwrap().is_empty());
        let commits = work_store.commits.lock().unwrap();
        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(commit.evaluations.len(), 1);
        assert_eq!(commit.evaluations[0].plan, PlanKind::Rsi);
        assert_eq!(commit.evaluations[0].release_id, release_id);
        assert_eq!(commit.evaluations[0].evaluated_closed_candle_time, 239_999);
        assert_eq!(commit.runtime_states.len(), 1);
        assert_eq!(commit.runtime_states[0].last_evaluated_close_time, 239_999);
        assert!(commit.signals.is_empty());
        assert!(commit.signal_events.is_empty());
        assert!(commit.directives.is_empty());
        let event = commit
            .monitoring_events
            .iter()
            .find(|event| event.event_type == "RSI_NON_LIVE_EVALUATION_SUPPRESSED")
            .expect("suppression monitoring event");
        assert_eq!(event.entity_id, Some(work_id));
        assert_eq!(
            event.payload,
            serde_json::json!({
                "key": expected_key,
                "workSource": source.as_str(),
                "releaseId": release_id,
                "evaluatedCloseTime": 239_999,
                "reason": "RSI_NON_LIVE_EVALUATION_SUPPRESSED",
            })
        );
        assert_eq!(
            commit
                .monitoring_events
                .iter()
                .filter(|event| event.event_type == "RSI_NON_LIVE_EVALUATION_SUPPRESSED")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn rsi_m1_neutral_live_work_suppresses_new_entry_evaluation() {
    let release_id = Uuid::now_v7();
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let rsi_profile = profile(release_id, PlanKind::Rsi, true);
    let expected_key = rsi_profile.key.clone();
    let (worker, work_store, _) = worker(
        release_id,
        ReleaseMode::PaperActive,
        vec![rsi_profile],
        HashMap::new(),
        GapMock::default(),
        vec![Arc::new(StubEvaluator {
            kind: PlanKind::Rsi,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        })],
    );

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert!(evaluator_order.lock().unwrap().is_empty());
    let commits = work_store.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].evaluations.len(), 1);
    assert_eq!(commits[0].runtime_states.len(), 1);
    assert_eq!(commits[0].runtime_states[0].key, expected_key);
    assert_eq!(
        commits[0].runtime_states[0].last_evaluated_close_time,
        239_999
    );
    let committed_state: PlanRuntimeState =
        serde_json::from_value(commits[0].runtime_states[0].state.clone()).unwrap();
    assert!(committed_state.active_signal.is_none());
    assert!(commits[0].signals.is_empty());
    assert!(commits[0].signal_events.is_empty());
    assert!(commits[0].directives.is_empty());
    assert!(commits[0].consumed_feedback.is_empty());
    let event = commits[0]
        .monitoring_events
        .iter()
        .find(|event| event.event_type == "RSI_M1_NEW_ENTRY_SUPPRESSED")
        .expect("RSI M1 new-entry suppression event");
    assert_eq!(
        event.payload,
        serde_json::json!({
            "key": expected_key,
            "workSource": WorkSource::Live.as_str(),
            "releaseId": release_id,
            "evaluatedCloseTime": 239_999,
            "reason": "RSI_M1_NEW_ENTRY_SUPPRESSED",
        })
    );
}

#[tokio::test]
async fn rsi_m1_active_live_work_delivers_feedback_and_commits_management_state() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, PlanKind::Rsi, true);
    let signal = stub_signal(&profile);
    let mut state = PlanRuntimeState::neutral(&profile);
    state.setup_state = SetupState::EntryAvailable;
    state.direction = signal.direction;
    state.active_signal = Some(signal.clone());
    let virtual_trade_id = Uuid::now_v7();
    let feedback = vec![PlanFeedback::ActualEntry {
        signal_id: signal.signal_id,
        virtual_trade_id,
        time: 240_001,
        price: Decimal::new(4_105, 0),
    }];
    let work_store = Arc::new(WorkMock::default());
    work_store.queue.lock().unwrap().push_back(work(release_id));
    *work_store.runtime_state.lock().unwrap() = Some(PlanRuntimeStateRecord {
        key: profile.key.clone(),
        release_id,
        state: serde_json::to_value(&state).unwrap(),
        last_evaluated_close_time: 200_000,
    });
    let feedback_store = Arc::new(FeedbackMock {
        pending: Mutex::new(feedback.clone()),
        pending_requests: Mutex::new(Vec::new()),
        signal_events: Mutex::new(Vec::new()),
    });
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let feedback_seen = Arc::new(Mutex::new(Vec::new()));
    let worker = AnalysisWorker {
        work: work_store.clone(),
        candles: Arc::new(CandleMock {
            values: Mutex::new(HashMap::new()),
        }),
        gaps: Arc::new(GapMock::default()),
        configurations: Arc::new(ConfigMock {
            mode: ReleaseMode::PaperActive,
            profiles: vec![profile],
        }),
        feedback: feedback_store.clone(),
        plans: PlanRegistry::new(vec![Arc::new(StubEvaluator {
            kind: PlanKind::Rsi,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: feedback_seen.clone(),
        }) as Arc<dyn PlanEvaluator>]),
    };

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert_eq!(*evaluator_order.lock().unwrap(), vec![PlanKind::Rsi]);
    assert_eq!(*feedback_seen.lock().unwrap(), vec![feedback.clone()]);
    assert_eq!(
        *feedback_store.pending_requests.lock().unwrap(),
        vec![signal.signal_id]
    );
    let commits = work_store.commits.lock().unwrap();
    assert_eq!(commits[0].consumed_feedback, feedback);
    let committed_state: PlanRuntimeState =
        serde_json::from_value(commits[0].runtime_states[0].state.clone()).unwrap();
    assert_eq!(
        committed_state.active_virtual_trade_id,
        Some(virtual_trade_id)
    );
    assert_eq!(
        committed_state
            .active_signal
            .as_ref()
            .map(|active| active.signal_id),
        Some(signal.signal_id)
    );
    assert!(
        commits[0]
            .monitoring_events
            .iter()
            .all(|event| event.event_type != "RSI_M1_NEW_ENTRY_SUPPRESSED")
    );
}

#[tokio::test]
async fn rsi_m5_neutral_live_work_preserves_evaluation_and_signal_output() {
    let release_id = Uuid::now_v7();
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let profile = profile_with_timeframe(release_id, PlanKind::Rsi, true, Timeframe::M5);
    let (worker, work_store, _) = worker(
        release_id,
        ReleaseMode::PaperActive,
        vec![profile],
        HashMap::new(),
        GapMock::default(),
        vec![Arc::new(StubEvaluator {
            kind: PlanKind::Rsi,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        })],
    );
    work_store.queue.lock().unwrap().clear();
    work_store
        .queue
        .lock()
        .unwrap()
        .push_back(work_with_timeframe(
            release_id,
            WorkSource::Live,
            Timeframe::M5,
        ));

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert_eq!(*evaluator_order.lock().unwrap(), vec![PlanKind::Rsi]);
    let commits = work_store.commits.lock().unwrap();
    assert_eq!(commits[0].runtime_states.len(), 1);
    assert_eq!(commits[0].signals.len(), 1);
    assert!(
        commits[0]
            .monitoring_events
            .iter()
            .all(|event| event.event_type != "RSI_M1_NEW_ENTRY_SUPPRESSED")
    );
}

#[tokio::test]
async fn non_rsi_m1_neutral_live_work_preserves_evaluation_and_signal_output() {
    let release_id = Uuid::now_v7();
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let (worker, work_store, _) = worker(
        release_id,
        ReleaseMode::PaperActive,
        vec![profile(release_id, PlanKind::Ict, true)],
        HashMap::new(),
        GapMock::default(),
        vec![Arc::new(StubEvaluator {
            kind: PlanKind::Ict,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        })],
    );

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert_eq!(*evaluator_order.lock().unwrap(), vec![PlanKind::Ict]);
    let commits = work_store.commits.lock().unwrap();
    assert_eq!(commits[0].runtime_states.len(), 1);
    assert_eq!(commits[0].signals.len(), 1);
    assert!(
        commits[0]
            .monitoring_events
            .iter()
            .all(|event| event.event_type != "RSI_M1_NEW_ENTRY_SUPPRESSED")
    );
}

#[tokio::test]
async fn non_live_rsi_work_does_not_load_or_consume_pending_feedback() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, PlanKind::Rsi, true);
    let signal = stub_signal(&profile);
    let mut state = PlanRuntimeState::neutral(&profile);
    state.setup_state = SetupState::EntryAvailable;
    state.direction = signal.direction;
    state.active_signal = Some(signal.clone());
    let pending_feedback = vec![PlanFeedback::ActualEntry {
        signal_id: signal.signal_id,
        virtual_trade_id: Uuid::now_v7(),
        time: 240_001,
        price: Decimal::new(4_105, 0),
    }];
    let work_store = Arc::new(WorkMock::default());
    work_store
        .queue
        .lock()
        .unwrap()
        .push_back(work_with_source(release_id, WorkSource::CatchUp));
    *work_store.runtime_state.lock().unwrap() = Some(PlanRuntimeStateRecord {
        key: profile.key.clone(),
        release_id,
        state: serde_json::to_value(&state).unwrap(),
        last_evaluated_close_time: 200_000,
    });
    let feedback_store = Arc::new(FeedbackMock {
        pending: Mutex::new(pending_feedback),
        pending_requests: Mutex::new(Vec::new()),
        signal_events: Mutex::new(Vec::new()),
    });
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let worker = AnalysisWorker {
        work: work_store.clone(),
        candles: Arc::new(CandleMock {
            values: Mutex::new(HashMap::new()),
        }),
        gaps: Arc::new(GapMock::default()),
        configurations: Arc::new(ConfigMock {
            mode: ReleaseMode::PaperActive,
            profiles: vec![profile],
        }),
        feedback: feedback_store.clone(),
        plans: PlanRegistry::new(vec![Arc::new(StubEvaluator {
            kind: PlanKind::Rsi,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        }) as Arc<dyn PlanEvaluator>]),
    };

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert!(evaluator_order.lock().unwrap().is_empty());
    assert!(feedback_store.pending_requests.lock().unwrap().is_empty());
    let commits = work_store.commits.lock().unwrap();
    assert!(commits[0].consumed_feedback.is_empty());
    assert!(commits[0].signals.is_empty());
}

#[tokio::test]
async fn non_live_rsi_work_applies_terminal_events_before_suppression() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, PlanKind::Rsi, true);
    let signal = stub_signal(&profile);
    let mut state = PlanRuntimeState::neutral(&profile);
    state.setup_state = SetupState::EntryAvailable;
    state.direction = signal.direction;
    state.active_signal = Some(signal.clone());
    let work_store = Arc::new(WorkMock::default());
    work_store
        .queue
        .lock()
        .unwrap()
        .push_back(work_with_source(release_id, WorkSource::CatchUp));
    *work_store.runtime_state.lock().unwrap() = Some(PlanRuntimeStateRecord {
        key: profile.key.clone(),
        release_id,
        state: serde_json::to_value(&state).unwrap(),
        last_evaluated_close_time: 200_000,
    });
    let feedback_store = Arc::new(FeedbackMock {
        pending: Mutex::new(Vec::new()),
        pending_requests: Mutex::new(Vec::new()),
        signal_events: Mutex::new(vec![analysis_domain::SignalEvent {
            event_id: Uuid::now_v7(),
            signal_id: signal.signal_id,
            sequence: 1,
            kind: analysis_domain::SignalEventKind::Expired,
            effective_at: 230_000,
            recorded_at: 230_001,
            virtual_trade_id: None,
            price: None,
            reason: Some("expired before replay".to_owned()),
        }]),
    });
    let evaluator_order = Arc::new(Mutex::new(Vec::new()));
    let worker = AnalysisWorker {
        work: work_store.clone(),
        candles: Arc::new(CandleMock {
            values: Mutex::new(HashMap::new()),
        }),
        gaps: Arc::new(GapMock::default()),
        configurations: Arc::new(ConfigMock {
            mode: ReleaseMode::PaperActive,
            profiles: vec![profile],
        }),
        feedback: feedback_store,
        plans: PlanRegistry::new(vec![Arc::new(StubEvaluator {
            kind: PlanKind::Rsi,
            fail: false,
            order: evaluator_order.clone(),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        }) as Arc<dyn PlanEvaluator>]),
    };

    assert_eq!(
        worker.run_once("worker", 1_000).await.unwrap(),
        WorkerStep::Completed
    );
    assert!(evaluator_order.lock().unwrap().is_empty());
    let commits = work_store.commits.lock().unwrap();
    let committed_state: PlanRuntimeState =
        serde_json::from_value(commits[0].runtime_states[0].state.clone()).unwrap();
    assert!(committed_state.active_signal.is_none());
    assert_eq!(
        commits[0].runtime_states[0].last_evaluated_close_time,
        239_999
    );
    assert!(commits[0].signals.is_empty());
    assert!(commits[0].signal_events.is_empty());
    assert!(commits[0].directives.is_empty());
}

#[tokio::test]
async fn non_rsi_plans_preserve_evaluation_for_every_work_source() {
    for plan in [
        PlanKind::Ict,
        PlanKind::Smc,
        PlanKind::Quasimodo,
        PlanKind::TrendBreakout,
        PlanKind::MeanReversion,
    ] {
        for source in [
            WorkSource::Live,
            WorkSource::CatchUp,
            WorkSource::DowntimeReconciled,
        ] {
            let release_id = Uuid::now_v7();
            let evaluator_order = Arc::new(Mutex::new(Vec::new()));
            let (worker, work_store, _) = worker(
                release_id,
                ReleaseMode::PaperActive,
                vec![profile(release_id, plan, true)],
                HashMap::new(),
                GapMock::default(),
                vec![Arc::new(StubEvaluator {
                    kind: plan,
                    fail: false,
                    order: evaluator_order.clone(),
                    readiness: Arc::new(Mutex::new(Vec::new())),
                    feedback_seen: Arc::new(Mutex::new(Vec::new())),
                })],
            );
            work_store.queue.lock().unwrap().clear();
            work_store
                .queue
                .lock()
                .unwrap()
                .push_back(work_with_source(release_id, source));

            assert_eq!(
                worker.run_once("worker", 1_000).await.unwrap(),
                WorkerStep::Completed
            );
            assert_eq!(*evaluator_order.lock().unwrap(), vec![plan]);
            let commits = work_store.commits.lock().unwrap();
            assert_eq!(commits[0].evaluations.len(), 1);
            assert_eq!(commits[0].runtime_states.len(), 1);
            assert_eq!(commits[0].signals.len(), 1);
        }
    }
}

#[tokio::test]
async fn run_keeps_polling_after_transient_iteration_error() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, PlanKind::Ict, true);
    let work_store = Arc::new(WorkMock {
        complete_failures_remaining: Mutex::new(1),
        ..WorkMock::default()
    });
    work_store.queue.lock().unwrap().push_back(work(release_id));
    work_store.queue.lock().unwrap().push_back(work(release_id));
    let worker = AnalysisWorker {
        work: work_store.clone(),
        candles: Arc::new(CandleMock {
            values: Mutex::new(HashMap::new()),
        }),
        gaps: Arc::new(GapMock::default()),
        configurations: Arc::new(ConfigMock {
            mode: ReleaseMode::PaperActive,
            profiles: vec![profile],
        }),
        feedback: Arc::new(FeedbackMock::default()),
        plans: PlanRegistry::new(vec![Arc::new(StubEvaluator {
            kind: PlanKind::Ict,
            fail: false,
            order: Arc::new(Mutex::new(Vec::new())),
            readiness: Arc::new(Mutex::new(Vec::new())),
            feedback_seen: Arc::new(Mutex::new(Vec::new())),
        }) as Arc<dyn PlanEvaluator>]),
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { worker.run("worker".to_owned(), shutdown_rx).await });

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if work_store.commits.lock().unwrap().len() == 1 {
                break;
            }
            assert!(
                !task.is_finished(),
                "worker stopped after one transient iteration failure"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    shutdown_tx.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(work_store.commits.lock().unwrap().len(), 1);
}

#[test]
fn release_fixture_keeps_lifecycle_explicit() {
    let release_id = Uuid::now_v7();
    let release = ConfigurationRelease {
        release_id,
        symbol: symbol(),
        mode: ReleaseMode::Research,
        lifecycle: ReleaseLifecycle::Active,
        validation_summary: serde_json::Value::Null,
        profiles: vec![profile(release_id, PlanKind::Rsi, true)],
    };
    assert_eq!(release.lifecycle, ReleaseLifecycle::Active);
}
