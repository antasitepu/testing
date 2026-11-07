use analysis_domain::{
    AnalysisWorkItem, CanonicalMarketSnapshot, HtfFilterMode, IctParameters, MaterializedProfile,
    MeanReversionParameters, PlanKind, PlanParameters, PlanProfileKey, QuasimodoParameters,
    Readiness, ReadinessReason, ReleaseMode, RsiPlanParameters, SmcParameters,
    TrendBreakoutParameters, VersionIdentity, WorkSource, WorkStatus,
};
use analysis_plans::{
    IctEvaluator, MeanReversionEvaluator, PlanEvaluator, PlanRuntimeState, QuasimodoEvaluator,
    RsiEvaluator, SmcEvaluator, TrendBreakoutEvaluator,
};
use analysis_service::{
    AnalysisCandleSource, AnalysisServiceError, AnalysisWorkStore, AnalysisWorker,
    ConfigurationRelease, ConfigurationStore, EvaluationCommit, GapStatusSource, PlanFeedbackStore,
    PlanRegistry, PlanRuntimeStateRecord, SnapshotAvailability, ValidationGate, WorkerStep,
};
use async_trait::async_trait;
use backtest_engine::{BacktestReplay, BacktestRunInput, FeeConfig, ReplayFrame, SlippageConfig};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CLOSE_TIME: i64 = 59_999;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn parameters(plan: PlanKind) -> PlanParameters {
    match plan {
        PlanKind::Ict => PlanParameters::Ict(IctParameters {
            higher_timeframe: None,
            fvg_selection: analysis_domain::IctFvgSelection::RelatedDisplacement,
            sweep_bias_mode: analysis_domain::IctSweepBiasMode::SameBias,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::ONE,
            minimum_rr: Decimal::ONE,
            bias_max_bars_valid: 10,
            sweep_max_bars_valid: 10,
            displacement_max_bars_valid: 10,
            entry_max_bars_valid: 10,
        }),
        PlanKind::Smc => PlanParameters::Smc(SmcParameters {
            higher_timeframe: None,
            htf_mode: HtfFilterMode::Off,
            weighted_countertrend_penalty: Decimal::ONE,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::ONE,
            minimum_rr: Decimal::ONE,
            max_bars_valid: 10,
        }),
        PlanKind::Rsi => PlanParameters::Rsi(RsiPlanParameters {
            rsi_period: 14,
            oversold_threshold: Decimal::new(30, 0),
            overbought_threshold: Decimal::new(70, 0),
            divergence_required: false,
            minimum_pivot_distance: 2,
            maximum_pivot_distance: 20,
            confirmed_swing_lookback: 10,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::ONE,
            target_rr: Decimal::TWO,
            max_bars_valid: 10,
            reversal_confirmation_bars: 2,
        }),
        PlanKind::Quasimodo => PlanParameters::Quasimodo(QuasimodoParameters {
            minimum_zone_ticks: 1,
            tolerance_atr_multiplier: Decimal::ONE,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::ONE,
            minimum_rr: Decimal::ONE,
            max_bars_valid: 10,
            max_structure_age_bars: 10,
            max_entry_distance_atr: Decimal::new(5, 0),
        }),
        PlanKind::TrendBreakout => PlanParameters::TrendBreakout(TrendBreakoutParameters {
            donchian_period: 20,
            ema_period: 50,
            adx_period: 14,
            minimum_adx: Decimal::ONE,
            higher_timeframe: None,
            atr_period: 14,
            initial_stop_atr_multiplier: Decimal::ONE,
            target_rr: Decimal::TWO,
            trailing_activation_r: Decimal::ONE,
            chandelier_period: 22,
            chandelier_atr_multiplier: Decimal::ONE,
        }),
        PlanKind::MeanReversion => PlanParameters::MeanReversion(MeanReversionParameters {
            bollinger_period: 20,
            bollinger_deviation: Decimal::TWO,
            zscore_period: 20,
            extreme_zscore: Decimal::TWO,
            adx_period: 14,
            maximum_adx: Decimal::new(25, 0),
            maximum_bandwidth: Decimal::ONE,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::ONE,
            minimum_rr: Decimal::ONE,
            max_bars_valid: 10,
        }),
    }
}

fn evaluator(plan: PlanKind) -> Arc<dyn PlanEvaluator> {
    match plan {
        PlanKind::Ict => Arc::new(IctEvaluator),
        PlanKind::Smc => Arc::new(SmcEvaluator),
        PlanKind::Rsi => Arc::new(RsiEvaluator),
        PlanKind::Quasimodo => Arc::new(QuasimodoEvaluator),
        PlanKind::TrendBreakout => Arc::new(TrendBreakoutEvaluator),
        PlanKind::MeanReversion => Arc::new(MeanReversionEvaluator),
    }
}

fn profile(release_id: Uuid, plan: PlanKind, timeframe: Timeframe) -> MaterializedProfile {
    let evaluator = evaluator(plan);
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan,
            target_timeframe: timeframe,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(
            release_id,
            evaluator.algorithm_version(),
            format!("{}-{}-v1", plan.as_str(), timeframe.as_str()),
        )
        .unwrap(),
        parameters: parameters(plan),
        required_timeframes: Vec::new(),
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

fn snapshot(timeframe: Timeframe) -> CanonicalMarketSnapshot {
    CanonicalMarketSnapshot::empty(
        symbol(),
        timeframe,
        Decimal::new(1, 2),
        CLOSE_TIME,
        Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory,
        },
    )
    .unwrap()
}

fn candle(timeframe: Timeframe) -> Candle {
    Candle::new(
        symbol(),
        timeframe,
        0,
        CLOSE_TIME,
        Decimal::new(100, 0),
        Decimal::new(101, 0),
        Decimal::new(99, 0),
        Decimal::new(100, 0),
    )
    .unwrap()
}

fn work(release_id: Uuid, timeframe: Timeframe) -> AnalysisWorkItem {
    AnalysisWorkItem {
        work_id: Uuid::now_v7(),
        symbol: symbol(),
        timeframe,
        canonical_close_time: CLOSE_TIME,
        release_id,
        source: WorkSource::CatchUp,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: 1,
        completed_at: None,
    }
}

#[derive(Default)]
struct WorkMock {
    queue: Mutex<VecDeque<AnalysisWorkItem>>,
    commits: Mutex<Vec<EvaluationCommit>>,
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
        self.commits.lock().unwrap().push(commit);
        Ok(())
    }

    async fn runtime_state(
        &self,
        _key: &PlanProfileKey,
        _release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError> {
        Ok(None)
    }

    async fn defer(&self, _work_id: Uuid, _lease_attempt: u32) -> Result<(), AnalysisServiceError> {
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

struct ConfigMock {
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
        Ok(ReleaseMode::Research)
    }

    async fn profiles_for_release(
        &self,
        _release_id: Uuid,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }
}

struct CandleMock;

#[async_trait]
impl AnalysisCandleSource for CandleMock {
    async fn snapshot(
        &self,
        _work: &AnalysisWorkItem,
        profile: &MaterializedProfile,
    ) -> Result<SnapshotAvailability, AnalysisServiceError> {
        Ok(SnapshotAvailability::Ready(snapshot(
            profile.key.target_timeframe,
        )))
    }
}

struct GapMock;

#[async_trait]
impl GapStatusSource for GapMock {
    async fn has_unresolved_gap(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _close_time: i64,
    ) -> Result<bool, AnalysisServiceError> {
        Ok(false)
    }

    async fn request_reconciliation(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _close_time: i64,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }
}

struct NoopFeedbackStore;

#[async_trait]
impl PlanFeedbackStore for NoopFeedbackStore {
    async fn pending_feedback(
        &self,
        _signal_id: Uuid,
    ) -> Result<Vec<analysis_domain::PlanFeedback>, AnalysisServiceError> {
        Ok(Vec::new())
    }

    async fn terminal_signal_events(
        &self,
        _signal_id: Uuid,
    ) -> Result<Vec<analysis_domain::SignalEvent>, AnalysisServiceError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn closed_candle_not_ready_fixture_matches_incremental_direct_and_backtest_paths() {
    let release_id = Uuid::now_v7();
    let profiles: Vec<_> = Timeframe::ALL
        .into_iter()
        .flat_map(|timeframe| {
            PlanKind::ALL
                .into_iter()
                .map(move |plan| profile(release_id, plan, timeframe))
        })
        .collect();
    let work = Arc::new(WorkMock::default());
    for timeframe in Timeframe::ALL {
        work.queue
            .lock()
            .unwrap()
            .push_back(work_item(release_id, timeframe));
    }
    let worker = AnalysisWorker {
        work: work.clone(),
        candles: Arc::new(CandleMock),
        gaps: Arc::new(GapMock),
        configurations: Arc::new(ConfigMock {
            profiles: profiles.clone(),
        }),
        feedback: Arc::new(NoopFeedbackStore),
        plans: PlanRegistry::new(PlanKind::ALL.into_iter().map(evaluator)),
    };

    for _ in Timeframe::ALL {
        assert_eq!(
            worker.run_once("parity", 1).await.unwrap(),
            WorkerStep::Completed
        );
    }

    let commits = work.commits.lock().unwrap();
    assert_eq!(commits.len(), 7);
    assert!(commits.iter().all(|commit| commit.signals.is_empty()));
    assert!(commits.iter().all(|commit| commit.signal_events.is_empty()));
    assert!(commits.iter().all(|commit| commit.directives.is_empty()));
    assert_eq!(
        commits
            .iter()
            .map(|commit| commit.evaluations.len())
            .sum::<usize>(),
        42
    );

    for profile in &profiles {
        let evaluator = evaluator(profile.key.plan);
        let state = PlanRuntimeState::neutral(profile);
        let direct = evaluator
            .evaluate(
                profile,
                &snapshot(profile.key.target_timeframe),
                &state,
                &[],
            )
            .unwrap();
        assert!(direct.signal.is_none());
        assert!(direct.events.is_empty());
        assert!(direct.directives.is_empty());
        assert_eq!(
            direct.next_state.readiness,
            snapshot(profile.key.target_timeframe).readiness
        );

        let replay = BacktestReplay::new(
            BacktestRunInput {
                symbol: symbol(),
                plan: profile.key.plan,
                target_timeframe: profile.key.target_timeframe,
                start_time: 0,
                end_time: CLOSE_TIME,
                release_id,
                algorithm_version: profile.versions.algorithm_version.clone(),
                parameter_version: profile.versions.parameter_version.clone(),
                fees: FeeConfig::default(),
                slippage: SlippageConfig::default(),
                account: None,
                code_commit_hash: "parity-fixture".to_owned(),
            },
            profile.clone(),
        );
        let replayed = replay
            .run(
                evaluator.as_ref(),
                vec![ReplayFrame {
                    candle: candle(profile.key.target_timeframe),
                    snapshot: snapshot(profile.key.target_timeframe),
                    directives: Vec::new(),
                    warmup: false,
                }],
            )
            .unwrap();
        assert!(replayed.trades.is_empty());
        assert!(replayed.executions.is_empty());
    }
}

fn work_item(release_id: Uuid, timeframe: Timeframe) -> AnalysisWorkItem {
    work(release_id, timeframe)
}
