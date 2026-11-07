use super::*;
use crate::tuning_protocol::{
    ChronologicalBoundaries, LOW_TIMEFRAMES, ProfileEvidence, StageMetrics, TuningStage,
    evaluate_promotion,
};
use analysis_core::{
    Displacement, FairValueGap, FairValueGapDirection, LiquidityPool, LiquiditySide,
    LiquiditySweep, StructureEvent,
};
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, QuasimodoParameters, ReadinessReason, RsiPlanParameters,
    TimeframeFacts, VersionIdentity,
};
use analysis_plans::{
    IctEvaluator, MeanReversionEvaluator, PlanEvaluator, QuasimodoEvaluator, RsiEvaluator,
    RsiV5Evaluator, SmcEvaluator, TrendBreakoutEvaluator,
};
use backtest_engine::{
    BacktestDiagnosticObservation, BacktestReplay, BacktestTrade, ExecutionKind, FeeConfig,
    ReplayFrame, SlippageConfig, compute_diagnostics, compute_metrics,
};
use market_domain::Candle;
use sqlx::postgres::PgPoolOptions;
use std::time::Instant;
use tokio::sync::OwnedMutexGuard;

#[derive(Clone)]
struct Candidate {
    label: String,
    parameters: PlanParameters,
}

#[derive(Clone)]
struct CandidateResult {
    plan: PlanKind,
    label: String,
    metrics: backtest_engine::BacktestMetrics,
    htf: HtfMetricCounts,
}

#[derive(Default)]
struct AggregateResult {
    trades: Vec<BacktestTrade>,
    candle_count: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HtfMetricCounts {
    aligned: u64,
    counter: u64,
    neutral: u64,
    missing: u64,
    off: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TuningFilter {
    plan: Option<PlanKind>,
    timeframe: Option<Timeframe>,
    candidate: Option<String>,
}

impl TuningFilter {
    fn includes_plan(&self, plan: PlanKind) -> bool {
        self.plan.is_none_or(|expected| expected == plan)
    }

    fn includes_timeframe(&self, timeframe: Timeframe) -> bool {
        self.timeframe.is_none_or(|expected| expected == timeframe)
    }

    fn includes_candidate(&self, label: &str) -> bool {
        self.candidate
            .as_ref()
            .is_none_or(|expected| label.to_ascii_lowercase().contains(expected))
    }
}

#[derive(Clone, Copy)]
enum IctSweepMode {
    SameBias,
    OppositeBias,
}

#[derive(Clone, Copy)]
enum IctFvgMode {
    RelatedDisplacement,
    NearestAfterDisplacement,
}

#[derive(Default)]
struct IctGateBreakdown {
    frames: u64,
    ready: u64,
    htf_structure: u64,
    sweep_same: u64,
    displacement_after_sweep: u64,
    structure_after_displacement: u64,
    related_unfilled_fvg: u64,
    invalidation_swing: u64,
    target_liquidity: u64,
    atr_positive: u64,
    rr_pass_08: u64,
    rr_pass_10: u64,
    rr_pass_12: u64,
    opposite_sweep: u64,
    opposite_displacement_after_sweep: u64,
    opposite_structure_after_displacement: u64,
    opposite_related_unfilled_fvg: u64,
    opposite_target_liquidity: u64,
    opposite_rr_pass_08: u64,
    nearest_unfilled_fvg: u64,
    nearest_rr_pass_08: u64,
}

#[derive(Default)]
struct IctPathProgress {
    sweep: bool,
    displacement: bool,
    structure: bool,
    fvg: bool,
    invalidation_swing: bool,
    target_liquidity: bool,
    atr_positive: bool,
    risk_reward: Option<Decimal>,
}

async fn tuning_test_pool() -> (PgPool, OwnedMutexGuard<()>) {
    let guard = TEST_DB_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    dotenvy::dotenv().ok();
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    MIGRATOR.run(&pool).await.expect("run migrations");
    sqlx::query(
            "TRUNCATE backtest_trades, backtest_runs, monitoring_events, \
             virtual_trade_events, virtual_trades, tracker_states, market_event_cursors, plan_feedback, \
             rejected_plan_candidates, trade_management_directives, signal_events, signals, \
             analysis_evaluations, plan_runtime_states, analysis_work_items, \
             analysis_runtime_state, analysis_profiles, configuration_releases, \
             market_sessions, sync_states, candles, markets RESTART IDENTITY CASCADE",
        )
        .execute(&pool)
        .await
        .expect("truncate tuning test tables");
    sqlx::query(
        "INSERT INTO markets \
             (symbol, enabled, status, price_tick, onboard_time, session_market) \
             VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .expect("seed market");
    (pool, guard)
}

async fn seed_tuning_session(pool: &PgPool, status: &str, start: i64, end: i64) {
    sqlx::query(
            "INSERT INTO market_sessions \
             (session_market, session_status, session_start_time, session_end_time, source_event_time) \
             VALUES ('COMMODITY', $1, $2, $3, $3)",
        )
        .bind(status)
        .bind(start)
        .bind(end)
        .execute(pool)
        .await
        .expect("seed tuning session");
}

async fn seed_tuning_candle(pool: &PgPool, open_time: i64, close: i64) {
    let candle = market_domain::Candle::new(
        Symbol::new("XAUUSDT").expect("symbol"),
        Timeframe::M1,
        open_time,
        open_time + Timeframe::M1.duration_ms() - 1,
        Decimal::new(close, 0),
        Decimal::new(close + 1, 0),
        Decimal::new(close - 1, 0),
        Decimal::new(close, 0),
    )
    .expect("candle");
    CandleRepository::new(pool.clone())
        .upsert_page(&[candle])
        .await
        .expect("seed tuning candle");
}

#[tokio::test]
async fn tuning_candle_loader_excludes_no_trading_session_rows() {
    let (pool, _guard) = tuning_test_pool().await;
    seed_tuning_session(&pool, "REGULAR", 0, 60_000).await;
    seed_tuning_session(&pool, "NO_TRADING", 60_000, 120_000).await;
    seed_tuning_candle(&pool, 0, 4_100).await;
    seed_tuning_candle(&pool, 60_000, 4_200).await;

    let candles = load_candles(&pool, &Symbol::new("XAUUSDT").unwrap(), Timeframe::M1, 10).await;

    assert_eq!(candles.len(), 1);
    assert_eq!(candles[0].open_time, 0);
    assert_eq!(candles[0].close, Decimal::new(4_100, 0));
    pool.close().await;
}

#[tokio::test]
async fn tuning_candle_loader_rejects_rows_without_session_history() {
    let (pool, _guard) = tuning_test_pool().await;
    let regular_friday_wib = 1_783_047_600_000;
    seed_tuning_candle(&pool, regular_friday_wib, 4_100).await;

    let loader = crate::tuning_dataset::TuningDatasetLoader::new(
        pool.clone(),
        "commodity-session-policy-v2",
        "commodity-overrides-v1",
    );
    let result = loader
        .load(&Symbol::new("XAUUSDT").unwrap(), Timeframe::M1, None, 10)
        .await;

    assert!(result.is_err());
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual execution diagnostic; requires DATABASE_URL"]
async fn print_active_execution_tracker_states_from_database() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect DATABASE_URL");
    let latest = sqlx::query(
        "SELECT close_time, close FROM candles \
             WHERE symbol = 'XAUUSDT' AND interval = '1m' \
             ORDER BY open_time DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("latest candle");
    println!(
        "latestClose={},close={}",
        latest.get::<i64, _>("close_time"),
        latest.get::<Decimal, _>("close")
    );
    println!("plan,tf,signalId,setupState,tradeState,effectiveAt,expiresAt,target,stop,state");
    let rows = sqlx::query(
        "SELECT s.plan, s.interval, s.signal_id, \
                    s.definition->>'effectiveAt' AS effective_at, \
                    s.definition->>'expiresAt' AS expires_at, \
                    s.definition->>'primaryTarget' AS primary_target, \
                    s.definition->>'stopLoss' AS stop_loss, \
                    ts.state->>'setupState' AS setup_state, \
                    ts.state->>'tradeState' AS trade_state, \
                    ts.state \
             FROM signals s \
             JOIN tracker_states ts ON ts.signal_id = s.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE s.symbol = 'XAUUSDT' \
               AND ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
             ORDER BY s.created_at DESC, s.signal_id DESC \
             LIMIT 20",
    )
    .fetch_all(&pool)
    .await
    .expect("active states");
    for row in rows {
        println!(
            "{},{},{},{},{},{},{},{},{},{}",
            row.get::<String, _>("plan"),
            row.get::<String, _>("interval"),
            row.get::<uuid::Uuid, _>("signal_id"),
            row.get::<String, _>("setup_state"),
            row.get::<String, _>("trade_state"),
            row.get::<String, _>("effective_at"),
            row.get::<Option<String>, _>("expires_at")
                .unwrap_or_else(|| "NULL".to_owned()),
            row.get::<String, _>("primary_target"),
            row.get::<String, _>("stop_loss"),
            row.get::<Value, _>("state"),
        );
    }
}

#[tokio::test]
#[ignore = "manual DB replay harness; requires DATABASE_URL and local candle history"]
async fn print_scalping_runtime_replay_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_REPLAY_MAX_BARS")
        .or_else(|_| std::env::var("GOLD_TUNING_MAX_BARS"))
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(2_000);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let scalping_timeframes = [Timeframe::M1, Timeframe::M5, Timeframe::M15];
    let mut load_timeframes = std::collections::BTreeSet::new();
    for timeframe in scalping_timeframes {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        load_timeframes.insert(timeframe);
        if let Some(higher) = next_higher_timeframe(timeframe) {
            load_timeframes.insert(higher);
        }
    }
    let mut candles_by_timeframe = std::collections::BTreeMap::new();
    for timeframe in load_timeframes {
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        println!(
            "REPLAY_LOAD,{},{},candles={}",
            symbol.as_str(),
            timeframe.as_str(),
            candles.len()
        );
        candles_by_timeframe.insert(timeframe, candles);
    }

    println!(
        "timeframe,plan,parameterVersion,frames,entries,closed,openAtEnd,cancelled,ambiguous,winRate,netR,expectancyR,profitFactor,maxDdR"
    );
    for timeframe in scalping_timeframes {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let Some(candles) = candles_by_timeframe.get(&timeframe) else {
            continue;
        };
        for plan in PlanKind::ALL {
            if !filter.includes_plan(plan) {
                continue;
            }
            let evaluator = runtime_replay_evaluator(plan);
            let release_id = uuid::Uuid::now_v7();
            let mut profile = default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                .expect("profile");
            profile.versions = VersionIdentity::new(
                release_id,
                evaluator.algorithm_version(),
                profile.versions.parameter_version.clone(),
            )
            .expect("versions");
            let warmup_bars = profile.minimum_history_bars.max(profile.warmup_bars);
            if candles.len() < warmup_bars {
                println!(
                    "{},{},{},0,0,0,0,0,0,0,0,0,0,0",
                    timeframe.as_str(),
                    plan.as_str(),
                    profile.versions.parameter_version
                );
                continue;
            }
            let higher = profile
                .required_timeframes
                .first()
                .and_then(|timeframe| candles_by_timeframe.get(timeframe));
            let frames =
                build_replay_frames_with_higher_for_plan(candles, higher, warmup_bars, plan);
            let input = backtest_engine::BacktestRunInput {
                symbol: symbol.clone(),
                plan,
                target_timeframe: timeframe,
                start_time: frames.first().unwrap().candle.open_time,
                end_time: frames.last().unwrap().candle.open_time,
                release_id,
                algorithm_version: profile.versions.algorithm_version.clone(),
                parameter_version: profile.versions.parameter_version.clone(),
                fees: FeeConfig::default(),
                slippage: SlippageConfig::default(),
                account: None,
                code_commit_hash: "local-scalping-runtime-replay".to_owned(),
            };
            let replay = BacktestReplay::new(input, profile.clone());
            let result = replay
                .run_ordered(evaluator.as_ref(), &frames)
                .unwrap_or_else(|error| {
                    panic!(
                        "runtime replay failed plan={} timeframe={}: {error}",
                        plan.as_str(),
                        timeframe.as_str()
                    )
                });
            let entries = result
                .executions
                .iter()
                .filter(|execution| {
                    matches!(
                        execution.kind,
                        ExecutionKind::Entry | ExecutionKind::ReverseEntry
                    )
                })
                .count();
            let cancelled = result
                .executions
                .iter()
                .filter(|execution| execution.kind == ExecutionKind::EntryCancelled)
                .count();
            let ambiguous = result
                .executions
                .iter()
                .filter(|execution| execution.ambiguous_intrabar)
                .count();
            println!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                timeframe.as_str(),
                plan.as_str(),
                profile.versions.parameter_version,
                frames.len(),
                entries,
                result.trades.len(),
                replay_open_at_end(&result.executions),
                cancelled,
                ambiguous,
                result.metrics.win_rate,
                result.metrics.net_return_r,
                result.metrics.expectancy_r,
                result.metrics.profit_factor,
                result.metrics.max_drawdown_r
            );
        }
    }
    pool.close().await;
}

#[test]
fn default_runtime_profiles_use_backtested_ict_rsi_qm_smc_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let ict = default_runtime_profile(release_id, symbol.clone(), PlanKind::Ict, Timeframe::M1)
        .expect("ict profile");
    assert_eq!(ict.versions.algorithm_version, "ICT-runtime-v4");
    assert_eq!(ict.versions.parameter_version, "ICT-1m-runtime-v3-pp1-pc1");
    let PlanParameters::Ict(ict) = ict.parameters else {
        panic!("expected ICT parameters");
    };
    assert_eq!(ict.fvg_selection, IctFvgSelection::NearestAfterDisplacement);
    assert_eq!(ict.sweep_bias_mode, IctSweepBiasMode::OppositeBias);
    assert_eq!(ict.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(ict.minimum_rr, Decimal::new(15, 1));
    assert_eq!(ict.entry_max_bars_valid, 10);

    let rsi = default_runtime_profile(release_id, symbol.clone(), PlanKind::Rsi, Timeframe::M1)
        .expect("rsi profile");
    assert_eq!(rsi.versions.algorithm_version, "RSI-runtime-v4");
    assert_eq!(rsi.versions.parameter_version, "RSI-1m-runtime-v3-pp1-pc1");
    assert_eq!(
        rsi.profit_protection,
        analysis_domain::ProfitProtectionParameters::default()
    );
    let PlanParameters::Rsi(rsi) = rsi.parameters else {
        panic!("expected RSI parameters");
    };
    assert_eq!(rsi.oversold_threshold, Decimal::new(35, 0));
    assert_eq!(rsi.overbought_threshold, Decimal::new(65, 0));
    assert!(!rsi.divergence_required);
    assert_eq!(rsi.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(rsi.target_rr, Decimal::new(15, 1));

    let qm = default_runtime_profile(
        release_id,
        symbol.clone(),
        PlanKind::Quasimodo,
        Timeframe::M1,
    )
    .expect("qm profile");
    assert_eq!(qm.versions.algorithm_version, "QUASIMODO-runtime-v4");
    assert_eq!(
        qm.versions.parameter_version,
        "QUASIMODO-1m-runtime-v2-pp1-pc1"
    );
    let PlanParameters::Quasimodo(qm) = qm.parameters else {
        panic!("expected QM parameters");
    };
    assert_eq!(qm.minimum_zone_ticks, 5);
    assert_eq!(qm.tolerance_atr_multiplier, Decimal::new(5, 1));
    assert_eq!(qm.atr_buffer_multiplier, Decimal::new(35, 2));
    assert_eq!(qm.minimum_rr, Decimal::new(15, 1));

    let smc = default_runtime_profile(release_id, symbol, PlanKind::Smc, Timeframe::M1)
        .expect("smc profile");
    assert_eq!(smc.versions.algorithm_version, "SMC-runtime-v4");
    assert_eq!(smc.versions.parameter_version, "SMC-1m-runtime-v2-pp1-pc1");
    let PlanParameters::Smc(smc) = smc.parameters else {
        panic!("expected SMC parameters");
    };
    assert_eq!(smc.htf_mode, HtfFilterMode::Weighted);
    assert_eq!(smc.atr_buffer_multiplier, Decimal::new(1, 1));
    assert_eq!(smc.minimum_rr, Decimal::new(1, 0));
}

#[test]
fn partial_close_runtime_profiles_use_isolated_versions() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    for (plan, expected_algorithm) in [
        (PlanKind::Ict, "ICT-runtime-v4"),
        (PlanKind::Smc, "SMC-runtime-v4"),
        (PlanKind::Rsi, "RSI-runtime-v4"),
        (PlanKind::Quasimodo, "QUASIMODO-runtime-v4"),
        (PlanKind::MeanReversion, "MEAN_REVERSION-runtime-v3"),
        (PlanKind::TrendBreakout, "TREND_BREAKOUT-runtime-v4"),
    ] {
        let profile = default_runtime_profile(release_id, symbol.clone(), plan, Timeframe::M1)
            .expect("runtime profile");
        assert_eq!(profile.versions.algorithm_version, expected_algorithm);
        assert!(profile.versions.parameter_version.ends_with("-pp1-pc1"));
    }
}

#[test]
fn default_runtime_profile_uses_tuned_ict_m5_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile = default_runtime_profile(release_id, symbol, PlanKind::Ict, Timeframe::M5)
        .expect("ict 5m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "ICT-5m-runtime-v4-pp1-pc1"
    );
    let PlanParameters::Ict(parameters) = profile.parameters else {
        panic!("expected ICT parameters");
    };
    assert_eq!(
        parameters.fvg_selection,
        IctFvgSelection::NearestAfterDisplacement
    );
    assert_eq!(parameters.sweep_bias_mode, IctSweepBiasMode::SameBias);
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(1, 1));
    assert_eq!(parameters.minimum_rr, Decimal::new(8, 1));
    assert_eq!(parameters.entry_max_bars_valid, 20);
}

#[test]
fn default_runtime_profile_uses_tuned_ict_m30_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile = default_runtime_profile(release_id, symbol, PlanKind::Ict, Timeframe::M30)
        .expect("ict 30m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "ICT-30m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Ict(parameters) = profile.parameters else {
        panic!("expected ICT parameters");
    };
    assert_eq!(
        parameters.fvg_selection,
        IctFvgSelection::NearestAfterDisplacement
    );
    assert_eq!(parameters.sweep_bias_mode, IctSweepBiasMode::OppositeBias);
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(1, 1));
    assert_eq!(parameters.minimum_rr, Decimal::new(1, 0));
    assert_eq!(parameters.entry_max_bars_valid, 20);
}

#[test]
fn default_runtime_profile_uses_tuned_smc_m5_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile = default_runtime_profile(release_id, symbol, PlanKind::Smc, Timeframe::M5)
        .expect("smc 5m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "SMC-5m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Smc(parameters) = profile.parameters else {
        panic!("expected SMC parameters");
    };
    assert_eq!(parameters.htf_mode, HtfFilterMode::Strict);
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(parameters.minimum_rr, Decimal::new(15, 1));
}

#[test]
fn default_runtime_profile_uses_tuned_smc_m15_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile = default_runtime_profile(release_id, symbol, PlanKind::Smc, Timeframe::M15)
        .expect("smc 15m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "SMC-15m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Smc(parameters) = profile.parameters else {
        panic!("expected SMC parameters");
    };
    assert_eq!(parameters.htf_mode, HtfFilterMode::Strict);
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(parameters.minimum_rr, Decimal::new(15, 1));
}

#[test]
fn default_runtime_profile_uses_tuned_rsi_h4_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile = default_runtime_profile(release_id, symbol, PlanKind::Rsi, Timeframe::H4)
        .expect("rsi h4 profile");
    assert_eq!(
        profile.versions.parameter_version,
        "RSI-4h-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Rsi(parameters) = profile.parameters else {
        panic!("expected RSI parameters");
    };
    assert_eq!(parameters.oversold_threshold, Decimal::new(35, 0));
    assert_eq!(parameters.overbought_threshold, Decimal::new(65, 0));
    assert!(!parameters.divergence_required);
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(35, 2));
    assert_eq!(parameters.target_rr, Decimal::new(125, 2));
}

#[test]
fn default_runtime_profile_uses_long_run_viable_rsi_m1_m5_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);

    let m1 = default_runtime_profile(release_id, symbol.clone(), PlanKind::Rsi, Timeframe::M1)
        .expect("rsi 1m profile");
    assert_eq!(m1.versions.parameter_version, "RSI-1m-runtime-v3-pp1-pc1");
    let PlanParameters::Rsi(m1) = m1.parameters else {
        panic!("expected RSI parameters");
    };
    assert_eq!(m1.oversold_threshold, Decimal::new(35, 0));
    assert_eq!(m1.overbought_threshold, Decimal::new(65, 0));
    assert!(!m1.divergence_required);
    assert_eq!(m1.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(m1.target_rr, Decimal::new(15, 1));

    let m5 = default_runtime_profile(release_id, symbol, PlanKind::Rsi, Timeframe::M5)
        .expect("rsi 5m profile");
    assert_eq!(m5.versions.parameter_version, "RSI-5m-runtime-v3-pp1-pc1");
    let PlanParameters::Rsi(m5) = m5.parameters else {
        panic!("expected RSI parameters");
    };
    assert_eq!(m5.oversold_threshold, Decimal::new(30, 0));
    assert_eq!(m5.overbought_threshold, Decimal::new(70, 0));
    assert!(!m5.divergence_required);
    assert_eq!(m5.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(m5.target_rr, Decimal::new(15, 1));
}

#[test]
fn default_runtime_profile_uses_tuned_qm_m5_m15_and_m30_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let m5 = default_runtime_profile(
        release_id,
        symbol.clone(),
        PlanKind::Quasimodo,
        Timeframe::M5,
    )
    .expect("qm 5m profile");
    assert_eq!(
        m5.versions.parameter_version,
        "QUASIMODO-5m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Quasimodo(m5) = m5.parameters else {
        panic!("expected QM parameters");
    };
    assert_eq!(m5.tolerance_atr_multiplier, Decimal::new(2, 1));
    assert_eq!(m5.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(m5.max_structure_age_bars, 5);
    assert_eq!(m5.max_entry_distance_atr, Decimal::new(5, 0));

    let m15 = default_runtime_profile(
        release_id,
        symbol.clone(),
        PlanKind::Quasimodo,
        Timeframe::M15,
    )
    .expect("qm 15m profile");
    assert_eq!(
        m15.versions.parameter_version,
        "QUASIMODO-15m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Quasimodo(m15) = m15.parameters else {
        panic!("expected QM parameters");
    };
    assert_eq!(m15.tolerance_atr_multiplier, Decimal::new(5, 1));
    assert_eq!(m15.atr_buffer_multiplier, Decimal::new(35, 2));
    assert_eq!(m15.max_structure_age_bars, 10);
    assert_eq!(m15.max_entry_distance_atr, Decimal::new(3, 0));

    let m30 = default_runtime_profile(release_id, symbol, PlanKind::Quasimodo, Timeframe::M30)
        .expect("qm 30m profile");
    assert_eq!(
        m30.versions.parameter_version,
        "QUASIMODO-30m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::Quasimodo(m30) = m30.parameters else {
        panic!("expected QM parameters");
    };
    assert_eq!(m30.tolerance_atr_multiplier, Decimal::new(35, 2));
    assert_eq!(m30.atr_buffer_multiplier, Decimal::new(2, 1));
    assert_eq!(m30.max_structure_age_bars, 10);
    assert_eq!(m30.max_entry_distance_atr, Decimal::new(5, 0));
}

#[test]
fn default_runtime_profile_uses_long_run_viable_trend_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let cases = [
        (
            Timeframe::M1,
            "TREND_BREAKOUT-1m-runtime-v2-pp1-pc1",
            Some(Timeframe::M5),
            Decimal::new(20, 0),
            Decimal::new(2, 0),
            Decimal::new(2, 0),
            Decimal::new(75, 2),
        ),
        (
            Timeframe::M5,
            "TREND_BREAKOUT-5m-runtime-v2-pp1-pc1",
            Some(Timeframe::M15),
            Decimal::new(20, 0),
            Decimal::new(15, 1),
            Decimal::new(15, 1),
            Decimal::new(75, 2),
        ),
        (
            Timeframe::M15,
            "TREND_BREAKOUT-15m-runtime-v2-pp1-pc1",
            None,
            Decimal::new(20, 0),
            Decimal::new(2, 0),
            Decimal::new(2, 0),
            Decimal::new(75, 2),
        ),
        (
            Timeframe::M30,
            "TREND_BREAKOUT-30m-runtime-v2-pp1-pc1",
            Some(Timeframe::H1),
            Decimal::new(25, 0),
            Decimal::new(2, 0),
            Decimal::new(2, 0),
            Decimal::new(1, 0),
        ),
        (
            Timeframe::H1,
            "TREND_BREAKOUT-1h-runtime-v2-pp1-pc1",
            None,
            Decimal::new(25, 0),
            Decimal::new(2, 0),
            Decimal::new(15, 1),
            Decimal::new(75, 2),
        ),
        (
            Timeframe::H4,
            "TREND_BREAKOUT-4h-runtime-v2-pp1-pc1",
            None,
            Decimal::new(20, 0),
            Decimal::new(15, 1),
            Decimal::new(2, 0),
            Decimal::new(75, 2),
        ),
    ];

    for (
        timeframe,
        parameter_version,
        higher_timeframe,
        minimum_adx,
        initial_stop_atr_multiplier,
        target_rr,
        trailing_activation_r,
    ) in cases
    {
        let profile = default_runtime_profile(
            release_id,
            symbol.clone(),
            PlanKind::TrendBreakout,
            timeframe,
        )
        .expect("trend profile");

        assert_eq!(profile.versions.parameter_version, parameter_version);
        let PlanParameters::TrendBreakout(parameters) = profile.parameters else {
            panic!("expected trend parameters");
        };
        assert_eq!(parameters.higher_timeframe, higher_timeframe);
        assert_eq!(parameters.minimum_adx, minimum_adx);
        assert_eq!(
            parameters.initial_stop_atr_multiplier,
            initial_stop_atr_multiplier
        );
        assert_eq!(parameters.target_rr, target_rr);
        assert_eq!(parameters.trailing_activation_r, trailing_activation_r);
    }
}

#[test]
fn default_runtime_profile_uses_tuned_mean_m1_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile =
        default_runtime_profile(release_id, symbol, PlanKind::MeanReversion, Timeframe::M1)
            .expect("mean 1m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "MEAN_REVERSION-1m-runtime-v3-pp1-pc1"
    );
    let PlanParameters::MeanReversion(parameters) = profile.parameters else {
        panic!("expected mean parameters");
    };
    assert_eq!(parameters.extreme_zscore, Decimal::new(15, 1));
    assert_eq!(parameters.maximum_adx, Decimal::new(20, 0));
    assert_eq!(parameters.maximum_bandwidth, Decimal::new(5, 2));
    assert_eq!(parameters.minimum_rr, Decimal::new(1, 0));
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(1, 1));
}

#[test]
fn default_runtime_profile_uses_tuned_mean_m5_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile =
        default_runtime_profile(release_id, symbol, PlanKind::MeanReversion, Timeframe::M5)
            .expect("mean 5m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "MEAN_REVERSION-5m-runtime-v2-pp1-pc1"
    );
    let PlanParameters::MeanReversion(parameters) = profile.parameters else {
        panic!("expected mean parameters");
    };
    assert_eq!(parameters.extreme_zscore, Decimal::new(15, 1));
    assert_eq!(parameters.maximum_adx, Decimal::new(20, 0));
    assert_eq!(parameters.maximum_bandwidth, Decimal::new(5, 2));
    assert_eq!(parameters.minimum_rr, Decimal::new(8, 1));
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(5, 2));
}

#[test]
fn default_runtime_profile_uses_tuned_mean_m15_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile =
        default_runtime_profile(release_id, symbol, PlanKind::MeanReversion, Timeframe::M15)
            .expect("mean 15m profile");

    assert_eq!(
        profile.versions.parameter_version,
        "MEAN_REVERSION-15m-runtime-v2-pp1-pc1"
    );
    let PlanParameters::MeanReversion(parameters) = profile.parameters else {
        panic!("expected mean parameters");
    };
    assert_eq!(parameters.extreme_zscore, Decimal::new(15, 1));
    assert_eq!(parameters.maximum_adx, Decimal::new(25, 0));
    assert_eq!(parameters.maximum_bandwidth, Decimal::new(5, 2));
    assert_eq!(parameters.minimum_rr, Decimal::new(1, 0));
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(1, 1));
}

#[test]
fn default_runtime_profile_uses_tuned_mean_h4_parameters() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(1);
    let profile =
        default_runtime_profile(release_id, symbol, PlanKind::MeanReversion, Timeframe::H4)
            .expect("mean 4h profile");

    assert_eq!(
        profile.versions.parameter_version,
        "MEAN_REVERSION-4h-runtime-v3-pp1-pc1"
    );
    let PlanParameters::MeanReversion(parameters) = profile.parameters else {
        panic!("expected mean parameters");
    };
    assert_eq!(parameters.extreme_zscore, Decimal::new(15, 1));
    assert_eq!(parameters.maximum_adx, Decimal::new(25, 0));
    assert_eq!(parameters.maximum_bandwidth, Decimal::new(5, 2));
    assert_eq!(parameters.minimum_rr, Decimal::new(1, 0));
    assert_eq!(parameters.atr_buffer_multiplier, Decimal::new(1, 1));
}

#[test]
fn mean_reversion_candidates_include_scalping_risk_variants() {
    let labels = mean_reversion_candidates()
        .into_iter()
        .map(|candidate| candidate.label)
        .collect::<Vec<_>>();

    assert!(labels.contains(&"mean-z1.5-adx20-bw0.05-rr0.8-atr0.05".to_owned()));
}

fn rsi_parameters_for_timeframe(timeframe: Timeframe) -> RsiPlanParameters {
    match default_parameters(PlanKind::Rsi, timeframe) {
        PlanParameters::Rsi(parameters) => parameters,
        _ => unreachable!("RSI defaults must contain RSI parameters"),
    }
}

#[test]
fn rsi_v5_grid_varies_only_confirmation_window() {
    let base = rsi_parameters_for_timeframe(Timeframe::M5);
    let candidates = rsi_v5_candidates(&base);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>(),
        vec!["rsi-v5-confirm1", "rsi-v5-confirm2", "rsi-v5-confirm3"]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| match &candidate.parameters {
                PlanParameters::Rsi(value) => value.reversal_confirmation_bars,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    for candidate in candidates {
        let PlanParameters::Rsi(mut parameters) = candidate.parameters else {
            unreachable!();
        };
        parameters.reversal_confirmation_bars = base.reversal_confirmation_bars;
        assert_eq!(parameters, base);
    }
}

#[test]
fn rsi_tuning_facts_omit_primitives_unused_by_rsi_plan() {
    let candles = tuning_candles(260, Timeframe::M5);
    let facts = build_tuning_facts_for_plan(&candles, PlanKind::Rsi).expect("rsi facts");

    assert!(has_fact_kind(&facts, FactKind::MarketClose));
    assert!(has_fact_kind(&facts, FactKind::Atr));
    assert!(has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(!has_fact_kind(&facts, FactKind::Liquidity));
    assert!(!has_fact_kind(&facts, FactKind::Donchian));
    assert!(!has_fact_kind(&facts, FactKind::Bollinger));
}

#[test]
fn qm_tuning_facts_omit_primitives_unused_by_quasimodo_plan() {
    let candles = tuning_candles(260, Timeframe::M15);
    let facts = build_tuning_facts_for_plan(&candles, PlanKind::Quasimodo).expect("qm facts");

    assert!(has_fact_kind(&facts, FactKind::MarketClose));
    assert!(has_fact_kind(&facts, FactKind::Atr));
    assert!(has_fact_kind(&facts, FactKind::SwingHigh));
    assert!(has_fact_kind(&facts, FactKind::SwingLow));
    assert!(!has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(!has_fact_kind(&facts, FactKind::Donchian));
    assert!(!has_fact_kind(&facts, FactKind::Bollinger));
}

#[test]
fn ict_smc_tuning_facts_omit_primitives_unused_by_ict_and_smc_plans() {
    let candles = tuning_candles(260, Timeframe::M5);
    let facts = build_ict_smc_tuning_facts(&candles).expect("ict smc facts");

    assert!(has_fact_kind(&facts, FactKind::MarketClose));
    assert!(has_fact_kind(&facts, FactKind::Atr));
    assert!(has_fact_kind(&facts, FactKind::SwingHigh));
    assert!(has_fact_kind(&facts, FactKind::SwingLow));
    assert!(has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(has_fact_kind(&facts, FactKind::Liquidity));
    assert!(!has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::Ema));
    assert!(!has_fact_kind(&facts, FactKind::Donchian));
    assert!(!has_fact_kind(&facts, FactKind::Bollinger));
}

#[test]
fn ict_smc_higher_tuning_facts_only_keep_structure_context() {
    let candles = tuning_candles(260, Timeframe::M15);
    let facts = build_ict_smc_higher_tuning_facts(&candles).expect("higher facts");

    assert!(has_fact_kind(&facts, FactKind::SwingHigh));
    assert!(has_fact_kind(&facts, FactKind::SwingLow));
    assert!(facts.iter().all(|fact| matches!(
        fact.id.kind,
        FactKind::SwingHigh | FactKind::SwingLow | FactKind::Structure
    )));
    assert!(!has_fact_kind(&facts, FactKind::Atr));
    assert!(!has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(!has_fact_kind(&facts, FactKind::Liquidity));
    assert!(!has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::Ema));
}

#[test]
fn trend_tuning_facts_only_keep_trend_primitives() {
    let candles = tuning_candles(260, Timeframe::M5);
    let facts = build_trend_tuning_facts(&candles).expect("trend facts");

    assert!(has_fact_kind(&facts, FactKind::MarketClose));
    assert!(has_fact_kind(&facts, FactKind::Atr));
    assert!(has_fact_kind(&facts, FactKind::Ema));
    assert!(has_fact_kind(&facts, FactKind::Adx));
    assert!(has_fact_kind(&facts, FactKind::Donchian));
    assert!(has_fact_kind(&facts, FactKind::Chandelier));
    assert!(!has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(!has_fact_kind(&facts, FactKind::Liquidity));
    assert!(!has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::Bollinger));
}

#[test]
fn mean_tuning_facts_only_keep_reversion_primitives() {
    let candles = tuning_candles(260, Timeframe::M5);
    let facts = build_mean_tuning_facts(&candles).expect("mean facts");

    assert!(has_fact_kind(&facts, FactKind::MarketClose));
    assert!(has_fact_kind(&facts, FactKind::Atr));
    assert!(has_fact_kind(&facts, FactKind::Adx));
    assert!(has_fact_kind(&facts, FactKind::Bollinger));
    assert!(!has_fact_kind(&facts, FactKind::FairValueGap));
    assert!(!has_fact_kind(&facts, FactKind::Liquidity));
    assert!(!has_fact_kind(&facts, FactKind::Rsi));
    assert!(!has_fact_kind(&facts, FactKind::Ema));
    assert!(!has_fact_kind(&facts, FactKind::Donchian));
}

#[test]
fn tuning_progress_line_is_csv_with_plan_timeframe_candidate_and_metrics() {
    assert_eq!(
        tuning_progress_line(
            "DONE",
            PlanKind::Quasimodo,
            Timeframe::M15,
            "qm-zone5",
            2_000,
            Some(12),
            Some(345)
        ),
        "TUNING,DONE,QUASIMODO,15m,qm-zone5,frames=2000,trades=12,durationMs=345"
    );
}

#[test]
fn tuning_result_line_includes_scope_and_timeframe() {
    let result = CandidateResult {
        plan: PlanKind::Ict,
        label: "ict-test".to_owned(),
        metrics: aggregate_metrics(Vec::new(), 200),
        htf: HtfMetricCounts::default(),
    };

    assert_eq!(
        tuning_result_header(),
        "scope,timeframe,plan,label,trades,winRate,netR,expectancyR,profitFactor,maxDdR,htfAligned,htfCounter,htfNeutral,htfMissing,htfOff"
    );
    assert_eq!(
        tuning_result_line("timeframe", Some(Timeframe::M5), &result),
        "timeframe,5m,ICT,ict-test,0,0,0,0,0,0,0,0,0,0,0"
    );
    assert_eq!(
        tuning_result_line("aggregate", None, &result),
        "aggregate,all,ICT,ict-test,0,0,0,0,0,0,0,0,0,0,0"
    );
}

#[test]
fn chronological_tuning_metrics_exclude_trades_crossing_split_boundaries() {
    let candles = tuning_candles(10, Timeframe::M1);
    let mut train = backtest_trade(1, PlanKind::Rsi, Timeframe::M1);
    train.entry_time = 60_000;
    train.exit_time = 120_000;
    let mut crossing = backtest_trade(2, PlanKind::Rsi, Timeframe::M1);
    crossing.entry_time = 300_000;
    crossing.exit_time = 360_000;
    let mut validation = backtest_trade(3, PlanKind::Rsi, Timeframe::M1);
    validation.entry_time = 360_000;
    validation.exit_time = 420_000;
    let mut test = backtest_trade(4, PlanKind::Rsi, Timeframe::M1);
    test.entry_time = 480_000;
    test.exit_time = 540_000;

    let metrics = chronological_tuning_metrics(&candles, &[train, crossing, validation, test])
        .expect("chronological metrics");

    assert_eq!(metrics.train.metrics.total_trades, 1);
    assert_eq!(metrics.validation.metrics.total_trades, 1);
    assert_eq!(metrics.test.metrics.total_trades, 1);
    assert_eq!(metrics.excluded_boundary_trades, 1);
    assert!(metrics.train.range.end() < metrics.validation.range.start());
    assert!(metrics.validation.range.end() < metrics.test.range.start());
    assert_eq!(
        tuning_split_result_line(
            "train",
            PlanKind::Rsi,
            Timeframe::M1,
            "rsi-test",
            &metrics.train,
            metrics.excluded_boundary_trades,
        ),
        "train,1m,RSI,rsi-test,0,359999,6,1,1,1,1,0,0,1,0,0,0,0,0,0,0,1,1,0,0,0,0"
    );
    assert_eq!(
        metrics.train.diagnostics.weighted_net_return_r,
        Decimal::ONE
    );
    assert_eq!(
        metrics.train.diagnostics.runner_contribution_r,
        Decimal::ONE
    );
}

#[test]
fn rsi_v4_v5_result_lines_include_split_scope_and_core_metrics() {
    let candles = tuning_candles(10, Timeframe::M5);
    let metrics = chronological_tuning_metrics(&candles, &[]).expect("chronological metrics");

    for (split, segment) in [
        ("train", &metrics.train),
        ("validation", &metrics.validation),
        ("test", &metrics.test),
    ] {
        let line = tuning_split_result_line(
            split,
            PlanKind::Rsi,
            Timeframe::M5,
            "rsi-v5-confirm1",
            segment,
            metrics.excluded_boundary_trades,
        );
        let fields = line.split(',').collect::<Vec<_>>();

        assert_eq!(fields[0], split);
        assert_eq!(fields[1], "5m");
        assert_eq!(fields[3], "rsi-v5-confirm1");
        assert_eq!(fields[6], segment.metrics.candle_count.to_string());
        assert_eq!(fields[7], "0");
        assert_eq!(fields[9], "0");
        assert_eq!(fields[10], "0");
        assert_eq!(fields[11], "0");
        assert_eq!(fields[12], "0");
    }
}

#[test]
fn rsi_comparison_ready_replay_frames_include_rsi_facts() {
    let candles = tuning_candles(205, Timeframe::M5);
    let frames = rsi_comparison_replay_frames(&candles, 200).collect::<Vec<_>>();
    let ready_frames = frames
        .iter()
        .filter(|frame| !frame.warmup)
        .collect::<Vec<_>>();

    assert!(!ready_frames.is_empty());
    assert!(ready_frames.iter().all(|frame| {
        matches!(frame.snapshot.readiness, Readiness::Ready)
            && has_fact_kind(&frame.snapshot.target.facts, FactKind::Rsi)
    }));
}

#[test]
fn runtime_fact_builder_exposes_stable_semantics_across_the_500_bar_slide() {
    let mut candles = tuning_candles(501, Timeframe::M1);
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    for (index, close_units) in [400_000_i64, 399_000, 402_000, 399_000, 400_000]
        .into_iter()
        .enumerate()
    {
        let open_time = i64::try_from(index).expect("index") * Timeframe::M1.duration_ms();
        candles[index] = Candle::new(
            symbol.clone(),
            Timeframe::M1,
            open_time,
            open_time + Timeframe::M1.duration_ms() - 1,
            Decimal::new(close_units, 2),
            Decimal::new(close_units + 20, 2),
            Decimal::new(close_units - 20, 2),
            Decimal::new(close_units, 2),
        )
        .expect("boundary candle");
    }

    let before = build_facts(&candles[..500]).expect("facts before slide");
    let after = build_facts(&candles[1..]).expect("facts after slide");
    let before_swings = before
        .iter()
        .filter(|fact| matches!(fact.id.kind, FactKind::SwingHigh | FactKind::SwingLow))
        .map(|fact| {
            serde_json::from_value::<ConfirmedSwing>(fact.value.clone()).expect("swing payload")
        })
        .collect::<Vec<_>>();
    let after_swings = after
        .iter()
        .filter(|fact| matches!(fact.id.kind, FactKind::SwingHigh | FactKind::SwingLow))
        .map(|fact| {
            serde_json::from_value::<ConfirmedSwing>(fact.value.clone()).expect("swing payload")
        })
        .collect::<Vec<_>>();
    let (before_swing, after_swing) = before_swings
        .iter()
        .find_map(|before_swing| {
            after_swings
                .iter()
                .find(|after_swing| {
                    before_swing.kind == after_swing.kind
                        && before_swing.pivot_open_time == after_swing.pivot_open_time
                        && before_swing.confirmed_at == after_swing.confirmed_at
                        && before_swing.price == after_swing.price
                        && before_swing.id.ordinal != after_swing.id.ordinal
                })
                .map(|after_swing| (before_swing, after_swing))
        })
        .expect("a retained swing must expose window-local ordinal drift");

    assert_eq!(before_swing.id.symbol, after_swing.id.symbol);
    assert_eq!(before_swing.id.timeframe, after_swing.id.timeframe);
    assert_eq!(before_swing.id.kind, after_swing.id.kind);
    assert_eq!(before_swing.id.confirmed_at, after_swing.id.confirmed_at);
    assert_eq!(
        before_swing.id.algorithm_version,
        after_swing.id.algorithm_version
    );
    assert_eq!(
        before_swing.id.parameter_version,
        after_swing.id.parameter_version
    );

    let retained_close_time = candles[499].close_time;
    let before_rsi = before
        .iter()
        .find(|fact| fact.id.kind == FactKind::Rsi && fact.id.confirmed_at == retained_close_time)
        .expect("RSI fact before slide");
    let after_rsi = after
        .iter()
        .find(|fact| fact.id.kind == FactKind::Rsi && fact.id.confirmed_at == retained_close_time)
        .expect("same RSI fact after slide");

    assert_eq!(before_rsi.id.ordinal, 499);
    assert_eq!(after_rsi.id.ordinal, 498);
    assert_eq!(before_rsi.id.symbol, after_rsi.id.symbol);
    assert_eq!(before_rsi.id.timeframe, after_rsi.id.timeframe);
    assert_eq!(before_rsi.id.kind, after_rsi.id.kind);
    assert_eq!(before_rsi.id.confirmed_at, after_rsi.id.confirmed_at);
    assert_eq!(
        before_rsi.id.algorithm_version,
        after_rsi.id.algorithm_version
    );
    assert_eq!(
        before_rsi.id.parameter_version,
        after_rsi.id.parameter_version
    );
}

#[test]
fn tuning_diagnostics_measure_only_post_exit_recovery_before_the_next_entry() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let candles = [
        market_domain::Candle::new(
            symbol.clone(),
            Timeframe::M1,
            180_000,
            239_999,
            Decimal::new(3_990, 0),
            Decimal::new(4_005, 0),
            Decimal::new(3_988, 0),
            Decimal::new(4_000, 0),
        )
        .unwrap(),
        market_domain::Candle::new(
            symbol.clone(),
            Timeframe::M1,
            240_000,
            299_999,
            Decimal::new(4_000, 0),
            Decimal::new(4_002, 0),
            Decimal::new(3_995, 0),
            Decimal::new(4_001, 0),
        )
        .unwrap(),
        market_domain::Candle::new(
            symbol,
            Timeframe::M1,
            300_000,
            359_999,
            Decimal::new(4_001, 0),
            Decimal::new(4_100, 0),
            Decimal::new(4_000, 0),
            Decimal::new(4_090, 0),
        )
        .unwrap(),
    ];
    let mut stopped = backtest_trade(1, PlanKind::Rsi, Timeframe::M1);
    stopped.entry_time = 60_000;
    stopped.exit_time = 120_000;
    stopped.entry_price = Decimal::new(4_000, 0);
    stopped.exit_price = Decimal::new(3_990, 0);
    stopped.initial_risk_price = Some(Decimal::new(10, 0));
    stopped.exit_reason = "STOP_LOSS".to_owned();
    stopped.mfe_r = Decimal::new(195, 2);
    stopped.tp2_execution_edge_r = Some(Decimal::TWO);
    let mut next = backtest_trade(2, PlanKind::Rsi, Timeframe::M1);
    next.entry_time = 300_000;

    let observations = tuning_diagnostic_observations(&candles, &[stopped.clone(), next]);
    let stopped = observations
        .iter()
        .find(|observation| observation.trade_id == stopped.trade_id)
        .expect("stopped trade observation");

    assert_eq!(stopped.post_stop_mfe_r, Some(Decimal::new(15, 1)));
    assert!(stopped.near_target_reversal);
}

#[test]
fn selected_tuning_candidate_resolves_runtime_baseline_and_grid_label() {
    let baseline =
        selected_tuning_candidate(PlanKind::MeanReversion, Timeframe::M5, "baseline-current")
            .expect("baseline candidate");
    assert_eq!(baseline.label, "baseline-current");
    assert_eq!(
        baseline.parameters,
        default_parameters(PlanKind::MeanReversion, Timeframe::M5)
    );

    let grid = selected_tuning_candidate(
        PlanKind::MeanReversion,
        Timeframe::M5,
        "mean-z2-adx25-bw0.05-rr1-atr0.1",
    )
    .expect("grid candidate");
    assert_eq!(grid.label, "mean-z2-adx25-bw0.05-rr1-atr0.1");
    assert!(selected_tuning_candidate(PlanKind::MeanReversion, Timeframe::M5, "missing").is_err());
}

#[test]
fn validation_candidate_labels_prepend_baseline_only_for_comparison() {
    assert_eq!(
        validation_candidate_labels("rsi-test", true),
        vec!["baseline-current", "rsi-test"]
    );
    assert_eq!(
        validation_candidate_labels("baseline-current", true),
        vec!["baseline-current"]
    );
    assert_eq!(
        validation_candidate_labels("rsi-test", false),
        vec!["rsi-test"]
    );
}

#[test]
fn staged_candidate_selection_never_uses_future_stage_labels() {
    let train = staged_candidates_from_values(
        PlanKind::MeanReversion,
        Timeframe::M1,
        TuningStage::Train,
        None,
        None,
    )
    .expect("train candidates");
    assert_eq!(train.len(), mean_reversion_candidates().len());

    let validation = staged_candidates_from_values(
        PlanKind::MeanReversion,
        Timeframe::M1,
        TuningStage::Validation,
        Some("mean-z1.5-adx20-bw0.05-rr1-atr0.1,mean-z2-adx25-bw0.08-rr1.2-atr0.2"),
        None,
    )
    .expect("validation candidates");
    assert_eq!(validation.len(), 2);
    assert!(
        validation
            .iter()
            .all(|candidate| candidate.label != "baseline-current")
    );

    let test = staged_candidates_from_values(
        PlanKind::MeanReversion,
        Timeframe::M1,
        TuningStage::Test,
        None,
        Some("mean-z1.5-adx20-bw0.05-rr1-atr0.1"),
    )
    .expect("test candidates");
    assert_eq!(test[0].label, "baseline-current");
    assert_eq!(test.len(), 2);
}

#[test]
fn staged_replay_end_never_crosses_its_chronological_boundary() {
    let split = ChronologicalBoundaries::new(101).expect("split");
    assert_eq!(staged_replay_end(TuningStage::Train, &split), 60);
    assert_eq!(staged_replay_end(TuningStage::Validation, &split), 80);
    assert_eq!(staged_replay_end(TuningStage::Test, &split), 101);
}

#[test]
fn runtime_default_contract_rejects_stale_profile_versions() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(2);
    let catalog = RuntimeProfileCatalog::current().expect("catalog");
    let mut profiles = PlanKind::ALL
        .into_iter()
        .flat_map(|plan| {
            let symbol = symbol.clone();
            Timeframe::ALL.into_iter().map(move |timeframe| {
                default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                    .expect("profile")
            })
        })
        .collect::<Vec<_>>();
    assert!(catalog.matches_release(release_id, &symbol, &profiles));

    let stale = profiles
        .iter_mut()
        .find(|profile| {
            profile.key.plan == PlanKind::Rsi && profile.key.target_timeframe == Timeframe::M1
        })
        .expect("rsi profile");
    stale.versions.parameter_version = "RSI-1m-runtime-v1".to_owned();

    assert!(!catalog.matches_release(release_id, &symbol, &profiles));
}

#[test]
fn runtime_profile_default_contract_rejects_stale_position_management() {
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let release_id = uuid::Uuid::from_u128(3);
    let catalog = RuntimeProfileCatalog::current().expect("catalog");
    let mut profiles = PlanKind::ALL
        .into_iter()
        .flat_map(|plan| {
            let symbol = symbol.clone();
            Timeframe::ALL.into_iter().map(move |timeframe| {
                default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                    .expect("profile")
            })
        })
        .collect::<Vec<_>>();
    assert!(catalog.matches_release(release_id, &symbol, &profiles));

    profiles[0].position_management.tp1_fraction = Decimal::new(4, 1);

    assert!(!catalog.matches_release(release_id, &symbol, &profiles));
}

#[test]
fn ict_smc_grid_includes_relaxed_sampling_candidates() {
    let ict = ict_candidates(Timeframe::M1);
    assert!(ict.iter().any(|candidate| {
        let PlanParameters::Ict(parameters) = &candidate.parameters else {
            return false;
        };
        parameters.higher_timeframe == Some(Timeframe::M5)
            && parameters.minimum_rr == Decimal::new(1, 0)
            && parameters.atr_buffer_multiplier == Decimal::new(1, 1)
            && parameters.entry_max_bars_valid == 20
    }));

    let smc = smc_candidates(Timeframe::M1);
    assert!(smc.iter().any(|candidate| {
        let PlanParameters::Smc(parameters) = &candidate.parameters else {
            return false;
        };
        parameters.htf_mode == HtfFilterMode::Off
            && parameters.minimum_rr == Decimal::new(1, 0)
            && parameters.atr_buffer_multiplier == Decimal::new(1, 1)
    }));
}

#[test]
fn trend_mean_grid_includes_runtime_safe_candidates() {
    let trend = trend_breakout_candidates(Timeframe::M1);
    assert!(trend.iter().any(|candidate| {
        candidate.label == "trend-htfon-adx20-stop1.5-rr1.5-trail1"
            && matches!(
                &candidate.parameters,
                PlanParameters::TrendBreakout(parameters)
                    if parameters.higher_timeframe == Some(Timeframe::M5)
                        && parameters.donchian_period == 20
                        && parameters.ema_period == 50
                        && parameters.adx_period == 14
                        && parameters.atr_period == 14
                        && parameters.chandelier_period == 22
            )
    }));
    assert!(trend.iter().any(|candidate| {
        candidate.label == "trend-htfoff-adx25-stop2-rr2-trail1"
            && matches!(
                &candidate.parameters,
                PlanParameters::TrendBreakout(parameters)
                    if parameters.higher_timeframe.is_none()
                        && parameters.minimum_adx == Decimal::new(25, 0)
                        && parameters.initial_stop_atr_multiplier == Decimal::new(2, 0)
                        && parameters.target_rr == Decimal::new(2, 0)
            )
    }));

    let mean = mean_reversion_candidates();
    assert!(mean.iter().any(|candidate| {
        candidate.label == "mean-z1.5-adx20-bw0.08-rr1-atr0.1"
            && matches!(
                &candidate.parameters,
                PlanParameters::MeanReversion(parameters)
                    if parameters.bollinger_period == 20
                        && parameters.bollinger_deviation == Decimal::new(2, 0)
                        && parameters.zscore_period == 20
                        && parameters.extreme_zscore == Decimal::new(15, 1)
                        && parameters.maximum_adx == Decimal::new(20, 0)
                        && parameters.maximum_bandwidth == Decimal::new(8, 2)
                        && parameters.minimum_rr == Decimal::new(1, 0)
                        && parameters.atr_buffer_multiplier == Decimal::new(1, 1)
            )
    }));
}

#[test]
fn trend_mean_tuning_loads_higher_timeframe_only_when_trend_is_in_scope() {
    let trend = tuning_filter_from_values(Some("trend"), Some("1m"), None);
    assert_eq!(
        trend_mean_tuning_load_timeframes(&trend),
        std::collections::BTreeSet::from([Timeframe::M1, Timeframe::M5])
    );

    let mean = tuning_filter_from_values(Some("mean"), Some("1m"), None);
    assert_eq!(
        trend_mean_tuning_load_timeframes(&mean),
        std::collections::BTreeSet::from([Timeframe::M1])
    );
}

#[test]
fn tuning_filter_accepts_plan_timeframe_and_candidate_aliases() {
    let filter = tuning_filter_from_values(Some("qm"), Some("1m"), Some("atr0.1"));

    assert!(filter.includes_plan(PlanKind::Quasimodo));
    assert!(!filter.includes_plan(PlanKind::Rsi));
    assert!(filter.includes_timeframe(Timeframe::M1));
    assert!(!filter.includes_timeframe(Timeframe::M5));
    assert!(filter.includes_candidate("qm-rr1.5-atr0.1"));
    assert!(!filter.includes_candidate("qm-rr1.5-atr0.2"));
}

#[test]
fn ict_smc_tuning_loads_higher_timeframe_for_filtered_target_only() {
    let filter = tuning_filter_from_values(Some("ict"), Some("1m"), None);

    assert_eq!(
        ict_smc_tuning_load_timeframes(&filter),
        std::collections::BTreeSet::from([Timeframe::M1, Timeframe::M5])
    );
    assert!(filter.includes_timeframe(Timeframe::M1));
    assert!(!filter.includes_timeframe(Timeframe::M5));
}

#[test]
fn htf_metric_counts_are_derived_from_closed_trades() {
    let mut aligned = backtest_trade(0, PlanKind::Ict, Timeframe::M1);
    aligned.htf_context = Some(analysis_domain::HtfContext {
        higher_timeframe: Some(Timeframe::M5),
        direction: Direction::Buy,
        alignment: analysis_domain::HtfAlignment::Aligned,
        source: "STRUCTURE".to_owned(),
    });
    let mut counter = backtest_trade(1, PlanKind::Ict, Timeframe::M1);
    counter.htf_context = Some(analysis_domain::HtfContext {
        higher_timeframe: Some(Timeframe::M5),
        direction: Direction::Sell,
        alignment: analysis_domain::HtfAlignment::Counter,
        source: "STRUCTURE".to_owned(),
    });

    let counts = htf_metric_counts(&[
        aligned,
        counter,
        backtest_trade(2, PlanKind::Ict, Timeframe::M1),
    ]);

    assert_eq!(counts.aligned, 1);
    assert_eq!(counts.counter, 1);
    assert_eq!(counts.missing, 1);
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_ict_smc_grid_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(500);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let mut candles_by_timeframe = std::collections::BTreeMap::new();
    let load_timeframes = ict_smc_tuning_load_timeframes(&filter);
    for timeframe in Timeframe::ALL {
        if !load_timeframes.contains(&timeframe) {
            continue;
        }
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() < tuning_warmup_bars(PlanKind::Ict, timeframe) {
            println!(
                "XAUUSDT,{},SKIP,candles={}",
                timeframe.as_str(),
                candles.len()
            );
            continue;
        }
        println!(
            "XAUUSDT,{},LOAD,candles={}",
            timeframe.as_str(),
            candles.len()
        );
        candles_by_timeframe.insert(timeframe, candles);
    }

    for plan in [PlanKind::Ict, PlanKind::Smc] {
        if !filter.includes_plan(plan) {
            continue;
        }
        let evaluator: Box<dyn PlanEvaluator> = match plan {
            PlanKind::Ict => Box::new(IctEvaluator),
            PlanKind::Smc => Box::new(SmcEvaluator),
            _ => unreachable!(),
        };
        let mut aggregates = std::collections::BTreeMap::<String, AggregateResult>::new();
        let mut timeframe_results =
            std::collections::BTreeMap::<Timeframe, Vec<CandidateResult>>::new();
        for timeframe in Timeframe::ALL {
            if !filter.includes_timeframe(timeframe) {
                continue;
            }
            let Some(candles) = candles_by_timeframe.get(&timeframe) else {
                continue;
            };
            let higher_timeframe = next_higher_timeframe(timeframe);
            let higher =
                higher_timeframe.and_then(|timeframe| candles_by_timeframe.get(&timeframe));
            let candidates = match plan {
                PlanKind::Ict => ict_candidates(timeframe),
                PlanKind::Smc => smc_candidates(timeframe),
                _ => unreachable!(),
            };
            for candidate in candidates {
                if !filter.includes_candidate(&candidate.label) {
                    continue;
                }
                let release_id = uuid::Uuid::now_v7();
                let mut profile =
                    default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                        .expect("profile");
                profile.versions = VersionIdentity::new(
                    release_id,
                    evaluator.algorithm_version(),
                    format!("{}-{}", candidate.label, timeframe.as_str()),
                )
                .expect("versions");
                profile.parameters = candidate.parameters;
                let input = backtest_engine::BacktestRunInput {
                    symbol: symbol.clone(),
                    plan,
                    target_timeframe: timeframe,
                    start_time: candles.first().unwrap().open_time,
                    end_time: candles.last().unwrap().open_time,
                    release_id,
                    algorithm_version: profile.versions.algorithm_version.clone(),
                    parameter_version: profile.versions.parameter_version.clone(),
                    fees: FeeConfig::default(),
                    slippage: SlippageConfig::default(),
                    account: None,
                    code_commit_hash: "local-ict-smc-tuning".to_owned(),
                };
                println!(
                    "{}",
                    tuning_progress_line(
                        "START",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        None,
                        None
                    )
                );
                let started = Instant::now();
                let replay = BacktestReplay::new(input, profile);
                let result = replay
                    .run_ordered_iter(
                        evaluator.as_ref(),
                        build_replay_frames_with_higher(
                            candles,
                            higher,
                            tuning_warmup_bars(plan, timeframe),
                        ),
                        candles.len() as u64,
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "replay failed plan={} timeframe={} label={}: {error}",
                            plan.as_str(),
                            timeframe.as_str(),
                            candidate.label
                        )
                    });
                let trades = result.trades;
                print_chronological_tuning_metrics(
                    plan,
                    timeframe,
                    &candidate.label,
                    candles,
                    &trades,
                );
                println!(
                    "{}",
                    tuning_progress_line(
                        "DONE",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        Some(trades.len() as u64),
                        Some(started.elapsed().as_millis())
                    )
                );
                let htf = htf_metric_counts(&trades);
                let metrics = aggregate_metrics(trades.clone(), candles.len() as u64);
                timeframe_results
                    .entry(timeframe)
                    .or_default()
                    .push(CandidateResult {
                        plan,
                        label: candidate.label.clone(),
                        metrics,
                        htf,
                    });
                let aggregate = aggregates.entry(candidate.label).or_default();
                aggregate.candle_count += candles.len() as u64;
                aggregate.trades.extend(trades);
            }
        }
        let mut results = aggregates
            .into_iter()
            .map(|(label, aggregate)| {
                let htf = htf_metric_counts(&aggregate.trades);
                CandidateResult {
                    plan,
                    label,
                    metrics: aggregate_metrics(aggregate.trades, aggregate.candle_count),
                    htf,
                }
            })
            .collect::<Vec<_>>();
        sort_candidate_results(&mut results);
        println!("{}", tuning_result_header());
        for timeframe in Timeframe::ALL {
            let Some(results) = timeframe_results.get_mut(&timeframe) else {
                continue;
            };
            sort_candidate_results(results);
            for result in results.iter().take(12) {
                println!(
                    "{}",
                    tuning_result_line("timeframe", Some(timeframe), result)
                );
            }
        }
        for result in results.into_iter().take(12) {
            println!("{}", tuning_result_line("aggregate", None, &result));
        }
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual RSI v4/v5 comparison; requires DATABASE_URL and local candle history"]
async fn print_rsi_v4_v5_grid_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(2_000);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");

    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() < tuning_warmup_bars(PlanKind::Rsi, timeframe) {
            println!(
                "XAUUSDT,{},SKIP,candles={}",
                timeframe.as_str(),
                candles.len()
            );
            continue;
        }
        run_rsi_comparison_for_timeframe(&symbol, timeframe, &candles);
    }
    pool.close().await;
}

fn run_rsi_comparison_for_timeframe(symbol: &Symbol, timeframe: Timeframe, candles: &[Candle]) {
    let base = rsi_parameters_for_timeframe(timeframe);
    let mut cases: Vec<(Candidate, Box<dyn PlanEvaluator>)> = vec![(
        Candidate {
            label: "rsi-v4-baseline".to_owned(),
            parameters: PlanParameters::Rsi(base.clone()),
        },
        Box::new(RsiEvaluator),
    )];
    cases.extend(rsi_v5_candidates(&base).into_iter().map(|candidate| {
        (
            candidate,
            Box::new(RsiV5Evaluator) as Box<dyn PlanEvaluator>,
        )
    }));

    for (candidate, evaluator) in cases {
        let release_id = uuid::Uuid::now_v7();
        let mut profile =
            default_runtime_profile(release_id, symbol.clone(), PlanKind::Rsi, timeframe)
                .expect("profile");
        profile.parameters = candidate.parameters;
        profile.versions = VersionIdentity::new(
            release_id,
            evaluator.algorithm_version(),
            candidate.label.clone(),
        )
        .expect("versions");
        let input = backtest_engine::BacktestRunInput {
            symbol: symbol.clone(),
            plan: PlanKind::Rsi,
            target_timeframe: timeframe,
            start_time: candles.first().expect("target candles").open_time,
            end_time: candles.last().expect("target candles").close_time,
            release_id,
            algorithm_version: profile.versions.algorithm_version.clone(),
            parameter_version: profile.versions.parameter_version.clone(),
            fees: FeeConfig::default(),
            slippage: SlippageConfig::default(),
            account: None,
            code_commit_hash: "local-rsi-v4-v5-comparison".to_owned(),
        };
        let replay = BacktestReplay::new(input, profile);
        let result = replay
            .run_ordered_iter(
                evaluator.as_ref(),
                rsi_comparison_replay_frames(candles, tuning_warmup_bars(PlanKind::Rsi, timeframe)),
                candles.len() as u64,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "RSI comparison failed timeframe={} label={}: {error}",
                    timeframe.as_str(),
                    candidate.label
                )
            });
        print_chronological_tuning_metrics(
            PlanKind::Rsi,
            timeframe,
            &candidate.label,
            candles,
            &result.trades,
        );
        let timeframe_result = CandidateResult {
            plan: PlanKind::Rsi,
            label: candidate.label,
            metrics: aggregate_metrics(result.trades, candles.len() as u64),
            htf: HtfMetricCounts::default(),
        };
        println!("{}", tuning_result_header());
        println!(
            "{}",
            tuning_result_line("timeframe", Some(timeframe), &timeframe_result)
        );
    }
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_primitive_counts_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(500);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    println!("timeframe,candles,swings,structure,displacement,fvg,relatedFvg,liquidity,sweep");
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() < 50 {
            println!("{},{},SKIP", timeframe.as_str(), candles.len());
            continue;
        }
        let facts = build_facts(&candles).expect("facts");
        let count = |kind| facts.iter().filter(|fact| fact.id.kind == kind).count();
        let swings = count(FactKind::SwingHigh) + count(FactKind::SwingLow);
        let related_fvg = facts
            .iter()
            .filter(|fact| fact.id.kind == FactKind::FairValueGap)
            .filter(|fact| {
                fact.value
                    .get("relatedDisplacementId")
                    .is_some_and(|value| !value.is_null())
            })
            .count();
        println!(
            "{},{},{},{},{},{},{},{},{}",
            timeframe.as_str(),
            candles.len(),
            swings,
            count(FactKind::Structure),
            count(FactKind::Displacement),
            count(FactKind::FairValueGap),
            related_fvg,
            count(FactKind::Liquidity),
            count(FactKind::LiquiditySweep)
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual ICT diagnostic harness; requires DATABASE_URL and local candle history"]
async fn print_ict_gate_breakdown_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(500);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let mut candles_by_timeframe = std::collections::BTreeMap::new();
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() >= tuning_warmup_bars(PlanKind::Ict, timeframe) {
            candles_by_timeframe.insert(timeframe, candles);
        }
    }

    println!(
        "timeframe,frames,ready,htfStructure,sweepSame,dispAfterSweep,structureAfterDisp,relatedUnfilledFvg,invalidationSwing,targetLiquidity,atrPositive,rr08,rr1,rr12,oppositeSweep,oppositeDisp,oppositeStructure,oppositeFvg,oppositeTarget,oppositeRr08,nearestFvg,nearestRr08"
    );
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let Some(candles) = candles_by_timeframe.get(&timeframe) else {
            println!("{},0,SKIP", timeframe.as_str());
            continue;
        };
        let higher_timeframe = next_higher_timeframe(timeframe);
        let higher = higher_timeframe.and_then(|timeframe| candles_by_timeframe.get(&timeframe));
        let frames = build_replay_frames_with_higher(
            candles,
            higher,
            tuning_warmup_bars(PlanKind::Ict, timeframe),
        );
        let mut breakdown = IctGateBreakdown::default();
        for frame in frames {
            breakdown.frames += 1;
            if frame.snapshot.readiness != Readiness::Ready {
                continue;
            }
            breakdown.ready += 1;
            let Some(direction) = latest_bias_direction(&frame.snapshot, higher_timeframe) else {
                continue;
            };
            breakdown.htf_structure += 1;

            let same = ict_path_progress(
                &frame.snapshot,
                direction,
                IctSweepMode::SameBias,
                IctFvgMode::RelatedDisplacement,
            );
            if same.sweep {
                breakdown.sweep_same += 1;
            }
            if same.displacement {
                breakdown.displacement_after_sweep += 1;
            }
            if same.structure {
                breakdown.structure_after_displacement += 1;
            }
            if same.fvg {
                breakdown.related_unfilled_fvg += 1;
            }
            if same.invalidation_swing {
                breakdown.invalidation_swing += 1;
            }
            if same.target_liquidity {
                breakdown.target_liquidity += 1;
            }
            if same.atr_positive {
                breakdown.atr_positive += 1;
            }
            if same
                .risk_reward
                .is_some_and(|risk_reward| risk_reward >= Decimal::new(8, 1))
            {
                breakdown.rr_pass_08 += 1;
            }
            if same
                .risk_reward
                .is_some_and(|risk_reward| risk_reward >= Decimal::new(1, 0))
            {
                breakdown.rr_pass_10 += 1;
            }
            if same
                .risk_reward
                .is_some_and(|risk_reward| risk_reward >= Decimal::new(12, 1))
            {
                breakdown.rr_pass_12 += 1;
            }

            let opposite = ict_path_progress(
                &frame.snapshot,
                direction,
                IctSweepMode::OppositeBias,
                IctFvgMode::RelatedDisplacement,
            );
            if opposite.sweep {
                breakdown.opposite_sweep += 1;
            }
            if opposite.displacement {
                breakdown.opposite_displacement_after_sweep += 1;
            }
            if opposite.structure {
                breakdown.opposite_structure_after_displacement += 1;
            }
            if opposite.fvg {
                breakdown.opposite_related_unfilled_fvg += 1;
            }
            if opposite.target_liquidity {
                breakdown.opposite_target_liquidity += 1;
            }
            if opposite
                .risk_reward
                .is_some_and(|risk_reward| risk_reward >= Decimal::new(8, 1))
            {
                breakdown.opposite_rr_pass_08 += 1;
            }

            let nearest = ict_path_progress(
                &frame.snapshot,
                direction,
                IctSweepMode::SameBias,
                IctFvgMode::NearestAfterDisplacement,
            );
            if nearest.fvg {
                breakdown.nearest_unfilled_fvg += 1;
            }
            if nearest
                .risk_reward
                .is_some_and(|risk_reward| risk_reward >= Decimal::new(8, 1))
            {
                breakdown.nearest_rr_pass_08 += 1;
            }
        }
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            timeframe.as_str(),
            breakdown.frames,
            breakdown.ready,
            breakdown.htf_structure,
            breakdown.sweep_same,
            breakdown.displacement_after_sweep,
            breakdown.structure_after_displacement,
            breakdown.related_unfilled_fvg,
            breakdown.invalidation_swing,
            breakdown.target_liquidity,
            breakdown.atr_positive,
            breakdown.rr_pass_08,
            breakdown.rr_pass_10,
            breakdown.rr_pass_12,
            breakdown.opposite_sweep,
            breakdown.opposite_displacement_after_sweep,
            breakdown.opposite_structure_after_displacement,
            breakdown.opposite_related_unfilled_fvg,
            breakdown.opposite_target_liquidity,
            breakdown.opposite_rr_pass_08,
            breakdown.nearest_unfilled_fvg,
            breakdown.nearest_rr_pass_08
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_low_timeframe_staged_tuning_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let plan = parse_tuning_plan(
        &std::env::var("GOLD_TUNING_PLAN").expect("GOLD_TUNING_PLAN is required"),
    )
    .expect("valid GOLD_TUNING_PLAN");
    let timeframe = parse_tuning_timeframe(
        &std::env::var("GOLD_TUNING_TIMEFRAME").expect("GOLD_TUNING_TIMEFRAME is required"),
    )
    .expect("valid GOLD_TUNING_TIMEFRAME");
    assert!(
        LOW_TIMEFRAMES.contains(&timeframe),
        "staged tuning is restricted to 1m, 5m, and 15m"
    );
    let stage = match std::env::var("GOLD_TUNING_STAGE")
        .expect("GOLD_TUNING_STAGE is required")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "train" => TuningStage::Train,
        "validation" => TuningStage::Validation,
        "test" => TuningStage::Test,
        _ => panic!("GOLD_TUNING_STAGE must be train, validation, or test"),
    };
    let candidates = staged_candidates_from_values(
        plan,
        timeframe,
        stage,
        std::env::var("GOLD_TUNING_CANDIDATES").ok().as_deref(),
        std::env::var("GOLD_TUNING_CANDIDATE").ok().as_deref(),
    )
    .expect("stage candidates");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(50_000);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
    let split = ChronologicalBoundaries::new(candles.len()).expect("chronological split");
    let replay_end = staged_replay_end(stage, &split);
    let higher_timeframe = if matches!(
        plan,
        PlanKind::Ict | PlanKind::Smc | PlanKind::TrendBreakout
    ) {
        next_higher_timeframe(timeframe)
    } else {
        None
    };
    let higher = if let Some(higher_timeframe) = higher_timeframe {
        Some(load_candles(&pool, &symbol, higher_timeframe, max_bars).await)
    } else {
        None
    };
    let evaluator = runtime_replay_evaluator(plan);
    let mut sessions = candidates
        .into_iter()
        .map(|candidate| {
            let release_id = uuid::Uuid::now_v7();
            let mut profile = default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                .expect("profile");
            profile.versions = VersionIdentity::new(
                release_id,
                evaluator.algorithm_version(),
                format!("staged-{}-{}", candidate.label, timeframe.as_str()),
            )
            .expect("versions");
            profile.parameters = candidate.parameters.clone();
            let input = backtest_engine::BacktestRunInput {
                symbol: symbol.clone(),
                plan,
                target_timeframe: timeframe,
                start_time: candles.first().expect("target candles").open_time,
                end_time: candles[replay_end - 1].close_time,
                release_id,
                algorithm_version: profile.versions.algorithm_version.clone(),
                parameter_version: profile.versions.parameter_version.clone(),
                fees: FeeConfig::default(),
                slippage: SlippageConfig::default(),
                account: None,
                code_commit_hash: "local-low-timeframe-staged-tuning".to_owned(),
            };
            let replay = BacktestReplay::new(input, profile);
            let session = replay
                .start_session(evaluator.as_ref())
                .expect("staged candidate session");
            (candidate, session)
        })
        .collect::<Vec<_>>();
    for frame in replay_frames_with_higher_for_plan(
        &candles,
        higher.as_ref(),
        tuning_warmup_bars(plan, timeframe),
        plan,
    )
    .take(replay_end)
    {
        for (_, session) in &mut sessions {
            session.process_frame(&frame).expect("staged replay frame");
        }
    }
    println!(
        "kind,stage,timeframe,plan,label,start,end,candles,trades,winRate,netR,expectancyR,profitFactor,maxDdR,excludedBoundaryTrades,hardStops,hardStopFrequency,postStopMfeR,nearTargetReversals,nearTargetReversalFrequency,partialFills,partialFillFrequency,weightedNetR,runnerContributionR,emergencyExitCount,emergencyExitEffectivenessR,suggestCloseCount,suggestCloseEffectivenessR"
    );
    for (candidate, session) in sessions {
        let result = session.finish(replay_end as u64);
        let metrics = chronological_tuning_metrics(&candles, &result.trades)
            .expect("chronological stage metrics");
        println!(
            "{}",
            staged_result_line(
                stage,
                plan,
                timeframe,
                &candidate.label,
                staged_segment(stage, &metrics),
                metrics.excluded_boundary_trades,
            )
        );
    }
    pool.close().await;
}

#[test]
#[ignore = "manual staged tuning decision adapter"]
fn print_low_timeframe_promotion_decision() {
    let evidence: ProfileEvidence = serde_json::from_str(
        &std::env::var("GOLD_TUNING_EVIDENCE_JSON").expect("GOLD_TUNING_EVIDENCE_JSON is required"),
    )
    .expect("valid GOLD_TUNING_EVIDENCE_JSON");
    println!("DECISION,{}", evaluate_promotion(&evidence).as_str());
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_selected_candidate_validation_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let plan_value = std::env::var("GOLD_TUNING_PLAN").expect("GOLD_TUNING_PLAN is required");
    let timeframe_value =
        std::env::var("GOLD_TUNING_TIMEFRAME").expect("GOLD_TUNING_TIMEFRAME is required");
    let candidate_label =
        std::env::var("GOLD_TUNING_CANDIDATE").expect("GOLD_TUNING_CANDIDATE is required");
    let plan = parse_tuning_plan(&plan_value).expect("valid GOLD_TUNING_PLAN");
    let timeframe = parse_tuning_timeframe(&timeframe_value).expect("valid GOLD_TUNING_TIMEFRAME");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(50_000);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
    let higher_timeframe = if matches!(
        plan,
        PlanKind::Ict | PlanKind::Smc | PlanKind::TrendBreakout
    ) {
        next_higher_timeframe(timeframe)
    } else {
        None
    };
    let higher = if let Some(higher_timeframe) = higher_timeframe {
        Some(load_candles(&pool, &symbol, higher_timeframe, max_bars).await)
    } else {
        None
    };
    let include_baseline = std::env::var("GOLD_TUNING_INCLUDE_BASELINE")
        .ok()
        .is_some_and(|value| value == "1");
    let evaluator = runtime_replay_evaluator(plan);
    let mut sessions = validation_candidate_labels(&candidate_label, include_baseline)
        .into_iter()
        .map(|label| {
            let candidate = selected_tuning_candidate(plan, timeframe, label)
                .expect("selected tuning candidate");
            let release_id = uuid::Uuid::now_v7();
            let mut profile = default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                .expect("profile");
            profile.versions = VersionIdentity::new(
                release_id,
                evaluator.algorithm_version(),
                format!("validation-{}-{}", candidate.label, timeframe.as_str()),
            )
            .expect("versions");
            profile.parameters = candidate.parameters.clone();
            let input = backtest_engine::BacktestRunInput {
                symbol: symbol.clone(),
                plan,
                target_timeframe: timeframe,
                start_time: candles.first().expect("target candles").open_time,
                end_time: candles.last().expect("target candles").close_time,
                release_id,
                algorithm_version: profile.versions.algorithm_version.clone(),
                parameter_version: profile.versions.parameter_version.clone(),
                fees: FeeConfig::default(),
                slippage: SlippageConfig::default(),
                account: None,
                code_commit_hash: "local-session-clean-validation".to_owned(),
            };
            let replay = BacktestReplay::new(input, profile);
            let session = replay
                .start_session(evaluator.as_ref())
                .expect("selected candidate session");
            (candidate, session)
        })
        .collect::<Vec<_>>();
    for frame in replay_frames_with_higher_for_plan(
        &candles,
        higher.as_ref(),
        tuning_warmup_bars(plan, timeframe),
        plan,
    ) {
        for (_, session) in &mut sessions {
            session
                .process_frame(&frame)
                .expect("selected candidate replay frame");
        }
    }
    println!("{}", tuning_result_header());
    for (candidate, session) in sessions {
        let result = session.finish(candles.len() as u64);
        println!(
            "{}",
            tuning_result_line(
                "validation",
                Some(timeframe),
                &CandidateResult {
                    plan,
                    label: candidate.label.clone(),
                    metrics: aggregate_metrics(result.trades.clone(), candles.len() as u64),
                    htf: htf_metric_counts(&result.trades),
                },
            )
        );
        print_chronological_tuning_metrics(
            plan,
            timeframe,
            &candidate.label,
            &candles,
            &result.trades,
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_rsi_qm_grid_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(2_000);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let mut candles_by_timeframe = std::collections::BTreeMap::new();
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() < 240 {
            println!(
                "XAUUSDT,{},SKIP,candles={}",
                timeframe.as_str(),
                candles.len()
            );
            continue;
        }
        println!(
            "XAUUSDT,{},LOAD,candles={}",
            timeframe.as_str(),
            candles.len()
        );
        candles_by_timeframe.insert(timeframe, candles);
    }

    for plan in [PlanKind::Rsi, PlanKind::Quasimodo] {
        if !filter.includes_plan(plan) {
            continue;
        }
        let evaluator: Box<dyn PlanEvaluator> = match plan {
            PlanKind::Rsi => Box::new(RsiEvaluator),
            PlanKind::Quasimodo => Box::new(QuasimodoEvaluator),
            _ => unreachable!(),
        };
        let candidates = match plan {
            PlanKind::Rsi => rsi_candidates(),
            PlanKind::Quasimodo => qm_candidates(),
            _ => unreachable!(),
        };
        let mut results = Vec::new();
        for candidate in candidates {
            if !filter.includes_candidate(&candidate.label) {
                continue;
            }
            let mut trades = Vec::new();
            let mut candle_count = 0;
            for timeframe in Timeframe::ALL {
                if !filter.includes_timeframe(timeframe) {
                    continue;
                }
                let Some(candles) = candles_by_timeframe.get(&timeframe) else {
                    continue;
                };
                candle_count += candles.len() as u64;
                let release_id = uuid::Uuid::now_v7();
                let mut profile =
                    default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                        .expect("profile");
                profile.versions = VersionIdentity::new(
                    release_id,
                    evaluator.algorithm_version(),
                    format!("{}-{}", candidate.label, timeframe.as_str()),
                )
                .expect("versions");
                profile.parameters = candidate.parameters.clone();
                let input = backtest_engine::BacktestRunInput {
                    symbol: symbol.clone(),
                    plan,
                    target_timeframe: timeframe,
                    start_time: candles.first().unwrap().open_time,
                    end_time: candles.last().unwrap().open_time,
                    release_id,
                    algorithm_version: profile.versions.algorithm_version.clone(),
                    parameter_version: profile.versions.parameter_version.clone(),
                    fees: FeeConfig::default(),
                    slippage: SlippageConfig::default(),
                    account: None,
                    code_commit_hash: "local-tuning".to_owned(),
                };
                println!(
                    "{}",
                    tuning_progress_line(
                        "START",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        None,
                        None
                    )
                );
                let started = Instant::now();
                let replay = BacktestReplay::new(input, profile);
                let result = replay
                    .run_ordered_iter(
                        evaluator.as_ref(),
                        build_replay_frames_for_plan(candles, 200, plan),
                        candles.len() as u64,
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "replay failed plan={} timeframe={} label={}: {error}",
                            plan.as_str(),
                            timeframe.as_str(),
                            candidate.label
                        )
                    });
                let trade_count = result.trades.len() as u64;
                print_chronological_tuning_metrics(
                    plan,
                    timeframe,
                    &candidate.label,
                    candles,
                    &result.trades,
                );
                println!(
                    "{}",
                    tuning_progress_line(
                        "DONE",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        Some(trade_count),
                        Some(started.elapsed().as_millis())
                    )
                );
                trades.extend(result.trades);
            }
            let htf = htf_metric_counts(&trades);
            let metrics = aggregate_metrics(trades, candle_count);
            results.push(CandidateResult {
                plan,
                label: candidate.label,
                metrics,
                htf,
            });
        }
        results.sort_by(|left, right| {
            right
                .metrics
                .net_return_r
                .cmp(&left.metrics.net_return_r)
                .then(right.metrics.expectancy_r.cmp(&left.metrics.expectancy_r))
                .then(right.metrics.total_trades.cmp(&left.metrics.total_trades))
        });
        println!(
            "plan,label,trades,winRate,netR,expectancyR,profitFactor,maxDdR,htfAligned,htfCounter,htfNeutral,htfMissing,htfOff"
        );
        for result in results.into_iter().take(12) {
            println!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                result.plan.as_str(),
                result.label,
                result.metrics.total_trades,
                result.metrics.win_rate,
                result.metrics.net_return_r,
                result.metrics.expectancy_r,
                result.metrics.profit_factor,
                result.metrics.max_drawdown_r,
                result.htf.aligned,
                result.htf.counter,
                result.htf.neutral,
                result.htf.missing,
                result.htf.off
            );
        }
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "manual tuning harness; requires DATABASE_URL and local candle history"]
async fn print_trend_mean_grid_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_TUNING_MAX_BARS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(2_000);
    let filter = tuning_filter_from_env();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect database");
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let mut candles_by_timeframe = std::collections::BTreeMap::new();
    for timeframe in trend_mean_tuning_load_timeframes(&filter) {
        let candles = load_candles(&pool, &symbol, timeframe, max_bars).await;
        if candles.len() < tuning_warmup_bars(PlanKind::TrendBreakout, timeframe) {
            println!(
                "XAUUSDT,{},SKIP,candles={}",
                timeframe.as_str(),
                candles.len()
            );
            continue;
        }
        println!(
            "XAUUSDT,{},LOAD,candles={}",
            timeframe.as_str(),
            candles.len()
        );
        candles_by_timeframe.insert(timeframe, candles);
    }

    for plan in [PlanKind::TrendBreakout, PlanKind::MeanReversion] {
        if !filter.includes_plan(plan) {
            continue;
        }
        let evaluator: Box<dyn PlanEvaluator> = match plan {
            PlanKind::TrendBreakout => Box::new(TrendBreakoutEvaluator),
            PlanKind::MeanReversion => Box::new(MeanReversionEvaluator),
            _ => unreachable!(),
        };
        let candidates = match plan {
            PlanKind::TrendBreakout => trend_breakout_candidates(Timeframe::M1),
            PlanKind::MeanReversion => mean_reversion_candidates(),
            _ => unreachable!(),
        };
        let mut aggregates = std::collections::BTreeMap::<String, AggregateResult>::new();
        let mut timeframe_results =
            std::collections::BTreeMap::<Timeframe, Vec<CandidateResult>>::new();
        for timeframe in Timeframe::ALL {
            if !filter.includes_timeframe(timeframe) {
                continue;
            }
            let Some(candles) = candles_by_timeframe.get(&timeframe) else {
                continue;
            };
            let higher_timeframe = if plan == PlanKind::TrendBreakout {
                next_higher_timeframe(timeframe)
            } else {
                None
            };
            let higher =
                higher_timeframe.and_then(|timeframe| candles_by_timeframe.get(&timeframe));
            let candidates = match plan {
                PlanKind::TrendBreakout => trend_breakout_candidates(timeframe),
                PlanKind::MeanReversion => candidates.clone(),
                _ => unreachable!(),
            };
            for candidate in candidates {
                if !filter.includes_candidate(&candidate.label) {
                    continue;
                }
                let release_id = uuid::Uuid::now_v7();
                let mut profile =
                    default_runtime_profile(release_id, symbol.clone(), plan, timeframe)
                        .expect("profile");
                profile.versions = VersionIdentity::new(
                    release_id,
                    evaluator.algorithm_version(),
                    format!("{}-{}", candidate.label, timeframe.as_str()),
                )
                .expect("versions");
                profile.parameters = candidate.parameters;
                let input = backtest_engine::BacktestRunInput {
                    symbol: symbol.clone(),
                    plan,
                    target_timeframe: timeframe,
                    start_time: candles.first().unwrap().open_time,
                    end_time: candles.last().unwrap().open_time,
                    release_id,
                    algorithm_version: profile.versions.algorithm_version.clone(),
                    parameter_version: profile.versions.parameter_version.clone(),
                    fees: FeeConfig::default(),
                    slippage: SlippageConfig::default(),
                    account: None,
                    code_commit_hash: "local-trend-mean-tuning".to_owned(),
                };
                println!(
                    "{}",
                    tuning_progress_line(
                        "START",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        None,
                        None
                    )
                );
                let started = Instant::now();
                let replay = BacktestReplay::new(input, profile);
                let result = replay
                    .run_ordered_iter(
                        evaluator.as_ref(),
                        replay_frames_with_higher_for_plan(
                            candles,
                            higher,
                            tuning_warmup_bars(plan, timeframe),
                            plan,
                        ),
                        candles.len() as u64,
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "replay failed plan={} timeframe={} label={}: {error}",
                            plan.as_str(),
                            timeframe.as_str(),
                            candidate.label
                        )
                    });
                let trade_count = result.trades.len() as u64;
                print_chronological_tuning_metrics(
                    plan,
                    timeframe,
                    &candidate.label,
                    candles,
                    &result.trades,
                );
                println!(
                    "{}",
                    tuning_progress_line(
                        "DONE",
                        plan,
                        timeframe,
                        &candidate.label,
                        candles.len() as u64,
                        Some(trade_count),
                        Some(started.elapsed().as_millis())
                    )
                );
                let htf = htf_metric_counts(&result.trades);
                let metrics = aggregate_metrics(result.trades.clone(), candles.len() as u64);
                timeframe_results
                    .entry(timeframe)
                    .or_default()
                    .push(CandidateResult {
                        plan,
                        label: candidate.label.clone(),
                        metrics,
                        htf,
                    });
                let aggregate = aggregates.entry(candidate.label).or_default();
                aggregate.candle_count += candles.len() as u64;
                aggregate.trades.extend(result.trades);
            }
        }
        println!(
            "scope,timeframe,plan,label,trades,winRate,netR,expectancyR,profitFactor,maxDdR,htfAligned,htfCounter,htfNeutral,htfMissing,htfOff"
        );
        for (timeframe, mut results) in timeframe_results {
            results.sort_by(|left, right| {
                right
                    .metrics
                    .net_return_r
                    .cmp(&left.metrics.net_return_r)
                    .then(right.metrics.expectancy_r.cmp(&left.metrics.expectancy_r))
                    .then(right.metrics.total_trades.cmp(&left.metrics.total_trades))
            });
            for result in results.into_iter().take(12) {
                println!(
                    "{}",
                    tuning_result_line("timeframe", Some(timeframe), &result)
                );
            }
        }
        let mut aggregate_results = aggregates
            .into_iter()
            .map(|(label, aggregate)| {
                let htf = htf_metric_counts(&aggregate.trades);
                let metrics = aggregate_metrics(aggregate.trades, aggregate.candle_count);
                CandidateResult {
                    plan,
                    label,
                    metrics,
                    htf,
                }
            })
            .collect::<Vec<_>>();
        aggregate_results.sort_by(|left, right| {
            right
                .metrics
                .net_return_r
                .cmp(&left.metrics.net_return_r)
                .then(right.metrics.expectancy_r.cmp(&left.metrics.expectancy_r))
                .then(right.metrics.total_trades.cmp(&left.metrics.total_trades))
        });
        for result in aggregate_results.into_iter().take(12) {
            println!("{}", tuning_result_line("aggregate", None, &result));
        }
    }
    pool.close().await;
}

async fn load_candles(
    pool: &sqlx::PgPool,
    symbol: &Symbol,
    timeframe: Timeframe,
    max_bars: i64,
) -> Vec<market_domain::Candle> {
    let overrides = crate::session_history::parse_session_overrides(
        include_str!("../../../../config/commodity-session-overrides.json"),
        "COMMODITY",
    )
    .expect("parse tuning session overrides");
    let dataset = crate::tuning_dataset::TuningDatasetLoader::new(
        pool.clone(),
        "commodity-session-policy-v2",
        overrides.version,
    )
    .load(
        symbol,
        timeframe,
        None,
        usize::try_from(max_bars).expect("positive tuning max bars"),
    )
    .await
    .expect("load session-clean tuning candles");
    println!(
        "{}",
        crate::tuning_dataset::dataset_audit_line(&dataset.audit)
    );
    dataset.candles
}

fn build_tuning_facts_for_plan(
    candles: &[market_domain::Candle],
    plan: PlanKind,
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    match plan {
        PlanKind::Ict | PlanKind::Smc => build_ict_smc_tuning_facts(candles),
        PlanKind::Rsi => build_rsi_tuning_facts(candles),
        PlanKind::Quasimodo => build_qm_tuning_facts(candles),
        PlanKind::TrendBreakout => build_trend_tuning_facts(candles),
        PlanKind::MeanReversion => build_mean_tuning_facts(candles),
    }
}

fn build_rsi_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let closes: Vec<_> = candles.iter().map(|candle| candle.close).collect();
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let structure_version = primitive_version(timeframe, "structure", "structure-runtime-v1");
    let momentum_version = primitive_version(timeframe, "momentum", "momentum-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let structure = detect_structure(
        candles,
        SwingParameters {
            left_bars: 2,
            right_bars: 2,
        },
        &structure_version.algorithm_version,
        &structure_version.parameter_version,
    )?;
    for swing in &structure.swings {
        push_fact(
            &mut facts,
            swing.id.clone(),
            swing,
            FactLifecycle::Confirmed,
        )?;
    }
    for event in &structure.events {
        push_fact(
            &mut facts,
            event.id.clone(),
            event,
            FactLifecycle::Confirmed,
        )?;
    }

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }

    let rsi_output = calculate_rsi(&closes, 14)?;
    for (index, value) in rsi_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_payload_fact(
                &mut facts,
                candles,
                index,
                FactKind::Rsi,
                json!({"value": value, "close": candles[index].close}),
                &momentum_version,
            )?;
        }
    }
    let pivots = rsi_pivots(
        candles,
        &structure.swings,
        &rsi_output.values,
        &momentum_version,
    );
    let divergences = detect_regular_divergence(
        &structure.swings,
        &pivots,
        2,
        20,
        &momentum_version.algorithm_version,
        &momentum_version.parameter_version,
    )?;
    for divergence in &divergences {
        push_fact(
            &mut facts,
            divergence.id.clone(),
            divergence,
            FactLifecycle::Confirmed,
        )?;
    }
    Ok(facts)
}

fn build_trend_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let closes: Vec<_> = candles.iter().map(|candle| candle.close).collect();
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let trend_version = primitive_version(timeframe, "trend", "trend-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }
    let ema = calculate_ema(&closes, 50, 5)?;
    for index in 0..candles.len() {
        if let (Some(value), Some(slope)) = (ema.values[index], ema.slopes[index]) {
            push_payload_fact(
                &mut facts,
                candles,
                index,
                FactKind::Ema,
                json!({"value": value, "slope": slope}),
                &trend_version,
            )?;
        }
    }
    let adx = calculate_adx(candles, 14)?;
    for (index, value) in adx.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Adx,
                *value,
                &trend_version,
            )?;
        }
    }
    let donchian = calculate_previous_donchian(candles, 20)?;
    for (index, value) in donchian.iter().enumerate() {
        if let Some(value) = value {
            let payload = DonchianRuntimeFact {
                period: value.period,
                upper: value.upper,
                lower: value.lower,
                source_end_time: value.source_end_time,
                close: candles[index].close,
            };
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Donchian, &trend_version)?,
                &payload,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    let chandelier = calculate_chandelier(candles, &atr_output.values, 22, Decimal::new(3, 0))?;
    for (index, value) in chandelier.iter().enumerate() {
        if let Some(value) = value {
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Chandelier, &trend_version)?,
                value,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    Ok(facts)
}

fn build_mean_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let closes: Vec<_> = candles.iter().map(|candle| candle.close).collect();
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let trend_version = primitive_version(timeframe, "trend", "trend-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }
    let adx = calculate_adx(candles, 14)?;
    for (index, value) in adx.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Adx,
                *value,
                &trend_version,
            )?;
        }
    }
    let bollinger = calculate_bollinger(&closes, 20, Decimal::new(2, 0))?;
    for (index, value) in bollinger.iter().enumerate() {
        if let Some(value) = value {
            let payload = BollingerRuntimeFact {
                period: value.period,
                deviation_multiplier: value.deviation_multiplier,
                upper: value.upper,
                middle: value.middle,
                lower: value.lower,
                deviation: value.deviation,
                bandwidth: value.bandwidth,
                zscore: value.zscore,
                close: candles[index].close,
                low: candles[index].low,
                high: candles[index].high,
            };
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Bollinger, &volatility_version)?,
                &payload,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    Ok(facts)
}

fn build_ict_smc_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let structure_version = primitive_version(timeframe, "structure", "structure-runtime-v1");
    let liquidity_version = primitive_version(timeframe, "liquidity", "liquidity-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let structure = detect_structure(
        candles,
        SwingParameters {
            left_bars: 2,
            right_bars: 2,
        },
        &structure_version.algorithm_version,
        &structure_version.parameter_version,
    )?;
    for swing in &structure.swings {
        push_fact(
            &mut facts,
            swing.id.clone(),
            swing,
            FactLifecycle::Confirmed,
        )?;
    }
    for event in &structure.events {
        push_fact(
            &mut facts,
            event.id.clone(),
            event,
            FactLifecycle::Confirmed,
        )?;
    }

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }
    if atr_output.values.iter().any(Option::is_some) {
        let displacements = detect_displacements_with_atr(
            candles,
            &atr_output.values,
            DisplacementParameters {
                minimum_body_atr: Decimal::new(8, 1),
                minimum_range_atr: Decimal::new(1, 0),
            },
            &structure_version.algorithm_version,
            &structure_version.parameter_version,
        )?;
        for displacement in &displacements {
            push_fact(
                &mut facts,
                displacement.id.clone(),
                displacement,
                FactLifecycle::Confirmed,
            )?;
            let window: Vec<_> = candles
                .iter()
                .filter(|candle| {
                    candle.close_time >= displacement.id.confirmed_at - timeframe.duration_ms() * 2
                        && candle.close_time <= displacement.id.confirmed_at
                })
                .cloned()
                .collect();
            if window.len() >= 3 {
                let later = &candles
                    [candles.partition_point(|c| c.close_time <= displacement.id.confirmed_at)..];
                for mut fvg in detect_fair_value_gaps(
                    &window,
                    Decimal::new(1, 2),
                    Some(displacement.id.clone()),
                    &liquidity_version.algorithm_version,
                    &liquidity_version.parameter_version,
                )? {
                    fvg.lifecycle =
                        analysis_core::fvg_lifecycle(fvg.direction, fvg.low, fvg.high, later);
                    push_fact(&mut facts, fvg.id.clone(), &fvg, fvg.lifecycle)?;
                }
            }
        }
    }
    for fvg in detect_fair_value_gaps(
        candles,
        Decimal::new(1, 2),
        None,
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )? {
        push_fact(&mut facts, fvg.id.clone(), &fvg, fvg.lifecycle)?;
    }
    let pools = detect_liquidity_pools(
        &structure.swings,
        Decimal::new(50, 2),
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )?;
    for pool in &pools {
        push_fact(&mut facts, pool.id.clone(), pool, pool.lifecycle)?;
    }
    let sweeps = detect_liquidity_sweeps(
        &pools,
        candles,
        Decimal::new(10, 2),
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )?;
    for sweep in &sweeps {
        push_fact(
            &mut facts,
            sweep.id.clone(),
            sweep,
            FactLifecycle::Confirmed,
        )?;
    }
    Ok(facts)
}

fn build_ict_smc_higher_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let structure_version = primitive_version(timeframe, "structure", "structure-runtime-v1");
    let structure = detect_structure(
        candles,
        SwingParameters {
            left_bars: 2,
            right_bars: 2,
        },
        &structure_version.algorithm_version,
        &structure_version.parameter_version,
    )?;
    for swing in &structure.swings {
        push_fact(
            &mut facts,
            swing.id.clone(),
            swing,
            FactLifecycle::Confirmed,
        )?;
    }
    for event in &structure.events {
        push_fact(
            &mut facts,
            event.id.clone(),
            event,
            FactLifecycle::Confirmed,
        )?;
    }
    Ok(facts)
}

fn build_trend_higher_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let closes: Vec<_> = candles.iter().map(|candle| candle.close).collect();
    let trend_version = primitive_version(timeframe, "trend", "trend-runtime-v1");
    let ema = calculate_ema(&closes, 50, 5)?;
    for index in 0..candles.len() {
        if let (Some(value), Some(slope)) = (ema.values[index], ema.slopes[index]) {
            push_payload_fact(
                &mut facts,
                candles,
                index,
                FactKind::Ema,
                json!({"value": value, "slope": slope}),
                &trend_version,
            )?;
        }
    }
    Ok(facts)
}

fn build_higher_tuning_facts_for_plan(
    candles: &[market_domain::Candle],
    plan: PlanKind,
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    match plan {
        PlanKind::Ict | PlanKind::Smc => build_ict_smc_higher_tuning_facts(candles),
        PlanKind::TrendBreakout => build_trend_higher_tuning_facts(candles),
        _ => Ok(Vec::new()),
    }
}

fn build_qm_tuning_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let structure_version = primitive_version(timeframe, "structure", "structure-runtime-v1");
    let liquidity_version = primitive_version(timeframe, "liquidity", "liquidity-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let structure = detect_structure(
        candles,
        SwingParameters {
            left_bars: 2,
            right_bars: 2,
        },
        &structure_version.algorithm_version,
        &structure_version.parameter_version,
    )?;
    for swing in &structure.swings {
        push_fact(
            &mut facts,
            swing.id.clone(),
            swing,
            FactLifecycle::Confirmed,
        )?;
    }
    for event in &structure.events {
        push_fact(
            &mut facts,
            event.id.clone(),
            event,
            FactLifecycle::Confirmed,
        )?;
    }

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }

    let pools = detect_liquidity_pools(
        &structure.swings,
        Decimal::new(50, 2),
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )?;
    for pool in &pools {
        push_fact(&mut facts, pool.id.clone(), pool, pool.lifecycle)?;
    }
    Ok(facts)
}

fn build_replay_frames_for_plan<'a>(
    candles: &'a [market_domain::Candle],
    warmup_bars: usize,
    plan: PlanKind,
) -> impl Iterator<Item = ReplayFrame> + 'a {
    candles.iter().enumerate().map(move |(index, candle)| {
        let ready = index + 1 >= warmup_bars;
        let start = index.saturating_sub(499);
        let facts = if ready {
            build_tuning_facts_for_plan(&candles[start..=index], plan).expect("facts")
        } else {
            Vec::new()
        };
        ReplayFrame {
            candle: candle.clone(),
            snapshot: CanonicalMarketSnapshot {
                symbol: candle.symbol.clone(),
                target_timeframe: candle.timeframe,
                price_tick: Decimal::new(1, 2),
                evaluated_closed_candle_time: candle.close_time,
                readiness: if ready {
                    Readiness::Ready
                } else {
                    Readiness::NotReady {
                        reason: ReadinessReason::InsufficientHistory,
                    }
                },
                target: TimeframeFacts {
                    timeframe: candle.timeframe,
                    evaluated_closed_candle_time: candle.close_time,
                    facts,
                },
                higher_timeframes: Vec::new(),
                primitive_versions: Vec::new(),
            },
            directives: Vec::new(),
            warmup: !ready,
        }
    })
}

fn build_replay_frames_with_higher_for_plan(
    candles: &[market_domain::Candle],
    higher_candles: Option<&Vec<market_domain::Candle>>,
    warmup_bars: usize,
    plan: PlanKind,
) -> Vec<ReplayFrame> {
    replay_frames_with_higher_for_plan(candles, higher_candles, warmup_bars, plan).collect()
}

fn rsi_comparison_replay_frames(
    candles: &[market_domain::Candle],
    warmup_bars: usize,
) -> impl Iterator<Item = ReplayFrame> + '_ {
    replay_frames_with_higher_for_plan(candles, None, warmup_bars, PlanKind::Rsi)
}

fn replay_frames_with_higher_for_plan<'a>(
    candles: &'a [market_domain::Candle],
    higher_candles: Option<&'a Vec<market_domain::Candle>>,
    warmup_bars: usize,
    plan: PlanKind,
) -> impl Iterator<Item = ReplayFrame> + 'a {
    let mut higher_fact_cache = None::<(usize, TimeframeFacts)>;
    candles.iter().enumerate().map(move |(index, candle)| {
        let ready = index + 1 >= warmup_bars;
        let start = index.saturating_sub(499);
        let facts = if ready {
            build_tuning_facts_for_plan(&candles[start..=index], plan).expect("facts")
        } else {
            Vec::new()
        };
        let higher_timeframes = higher_candles
            .and_then(|higher| {
                let end = higher
                    .partition_point(|higher_candle| higher_candle.close_time <= candle.close_time)
                    .checked_sub(1)?;
                let start = end.saturating_sub(499);
                if end + 1 < warmup_bars {
                    return None;
                }
                if let Some((cached_end, cached)) = &higher_fact_cache
                    && *cached_end == end
                {
                    return Some(cached.clone());
                }
                let higher_window = &higher[start..=end];
                let higher_facts =
                    build_higher_tuning_facts_for_plan(higher_window, plan).expect("higher facts");
                let latest = higher_window.last().expect("higher window");
                let facts = TimeframeFacts {
                    timeframe: latest.timeframe,
                    evaluated_closed_candle_time: latest.close_time,
                    facts: higher_facts,
                };
                higher_fact_cache = Some((end, facts.clone()));
                Some(facts)
            })
            .into_iter()
            .collect();
        ReplayFrame {
            candle: candle.clone(),
            snapshot: CanonicalMarketSnapshot {
                symbol: candle.symbol.clone(),
                target_timeframe: candle.timeframe,
                price_tick: Decimal::new(1, 2),
                evaluated_closed_candle_time: candle.close_time,
                readiness: if ready {
                    Readiness::Ready
                } else {
                    Readiness::NotReady {
                        reason: ReadinessReason::InsufficientHistory,
                    }
                },
                target: TimeframeFacts {
                    timeframe: candle.timeframe,
                    evaluated_closed_candle_time: candle.close_time,
                    facts,
                },
                higher_timeframes,
                primitive_versions: Vec::new(),
            },
            directives: Vec::new(),
            warmup: !ready,
        }
    })
}

fn has_fact_kind(facts: &[ConfirmedFact<Value>], kind: FactKind) -> bool {
    facts.iter().any(|fact| fact.id.kind == kind)
}

fn tuning_progress_line(
    stage: &str,
    plan: PlanKind,
    timeframe: Timeframe,
    candidate: &str,
    frames: u64,
    trades: Option<u64>,
    duration_ms: Option<u128>,
) -> String {
    let trade_segment = trades
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let duration_segment = duration_ms
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned());
    format!(
        "TUNING,{},{},{},{},frames={},trades={},durationMs={}",
        stage,
        plan.as_str(),
        timeframe.as_str(),
        candidate,
        frames,
        trade_segment,
        duration_segment
    )
}

fn runtime_replay_evaluator(plan: PlanKind) -> Box<dyn PlanEvaluator> {
    match plan {
        PlanKind::Ict => Box::new(IctEvaluator),
        PlanKind::Smc => Box::new(SmcEvaluator),
        PlanKind::Rsi => Box::new(RsiEvaluator),
        PlanKind::Quasimodo => Box::new(QuasimodoEvaluator),
        PlanKind::TrendBreakout => Box::new(TrendBreakoutEvaluator),
        PlanKind::MeanReversion => Box::new(MeanReversionEvaluator),
    }
}

fn replay_open_at_end(executions: &[backtest_engine::SimulatedExecution]) -> i64 {
    let mut open_count = 0_i64;
    for execution in executions {
        match execution.kind {
            ExecutionKind::Entry | ExecutionKind::ReverseEntry => {
                open_count += 1;
            }
            ExecutionKind::ExitAtNextOpen
            | ExecutionKind::SuggestClose
            | ExecutionKind::EmergencyExit
            | ExecutionKind::ReverseExit
            | ExecutionKind::StopLoss
            | ExecutionKind::TakeProfit => {
                open_count = open_count.saturating_sub(1);
            }
            ExecutionKind::PartialTakeProfit | ExecutionKind::EntryCancelled => {}
        }
    }
    open_count
}

fn tuning_result_header() -> &'static str {
    "scope,timeframe,plan,label,trades,winRate,netR,expectancyR,profitFactor,maxDdR,htfAligned,htfCounter,htfNeutral,htfMissing,htfOff"
}

fn tuning_result_line(
    scope: &str,
    timeframe: Option<Timeframe>,
    result: &CandidateResult,
) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        scope,
        timeframe.map_or("all", |timeframe| timeframe.as_str()),
        result.plan.as_str(),
        result.label,
        result.metrics.total_trades,
        result.metrics.win_rate,
        result.metrics.net_return_r,
        result.metrics.expectancy_r,
        result.metrics.profit_factor,
        result.metrics.max_drawdown_r,
        result.htf.aligned,
        result.htf.counter,
        result.htf.neutral,
        result.htf.missing,
        result.htf.off
    )
}

fn tuning_split_result_line(
    split: &str,
    plan: PlanKind,
    timeframe: Timeframe,
    label: &str,
    segment: &ChronologicalSegmentMetrics,
    excluded_boundary_trades: u64,
) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        split,
        timeframe.as_str(),
        plan.as_str(),
        label,
        segment.range.start(),
        segment.range.end(),
        segment.metrics.candle_count,
        segment.metrics.total_trades,
        segment.metrics.win_rate,
        segment.metrics.net_return_r,
        segment.metrics.expectancy_r,
        segment.metrics.profit_factor,
        segment.metrics.max_drawdown_r,
        excluded_boundary_trades,
        segment.diagnostics.hard_stop_count,
        segment.diagnostics.hard_stop_frequency,
        segment.diagnostics.post_stop_mfe_r,
        segment.diagnostics.near_target_reversal_count,
        segment.diagnostics.near_target_reversal_frequency,
        segment.diagnostics.partial_fill_count,
        segment.diagnostics.partial_fill_frequency,
        segment.diagnostics.weighted_net_return_r,
        segment.diagnostics.runner_contribution_r,
        segment.diagnostics.emergency_exit_count,
        segment.diagnostics.emergency_exit_effectiveness_r,
        segment.diagnostics.suggest_close_count,
        segment.diagnostics.suggest_close_effectiveness_r,
    )
}

fn print_chronological_tuning_metrics(
    plan: PlanKind,
    timeframe: Timeframe,
    label: &str,
    candles: &[market_domain::Candle],
    trades: &[BacktestTrade],
) {
    let split =
        chronological_tuning_metrics(candles, trades).expect("valid chronological tuning split");
    println!(
        "split,timeframe,plan,label,start,end,candles,trades,winRate,netR,expectancyR,profitFactor,maxDdR,excludedBoundaryTrades,hardStops,hardStopFrequency,postStopMfeR,nearTargetReversals,nearTargetReversalFrequency,partialFills,partialFillFrequency,weightedNetR,runnerContributionR,emergencyExitCount,emergencyExitEffectivenessR,suggestCloseCount,suggestCloseEffectivenessR"
    );
    for (name, segment) in [
        ("train", &split.train),
        ("validation", &split.validation),
        ("test", &split.test),
    ] {
        println!(
            "{}",
            tuning_split_result_line(
                name,
                plan,
                timeframe,
                label,
                segment,
                split.excluded_boundary_trades,
            )
        );
    }
}

fn staged_segment(
    stage: TuningStage,
    metrics: &ChronologicalTuningMetrics,
) -> &ChronologicalSegmentMetrics {
    match stage {
        TuningStage::Train => &metrics.train,
        TuningStage::Validation => &metrics.validation,
        TuningStage::Test => &metrics.test,
    }
}

fn stage_metrics(segment: &ChronologicalSegmentMetrics) -> StageMetrics {
    StageMetrics {
        total_trades: segment.metrics.total_trades,
        win_rate: segment.metrics.win_rate,
        net_r: segment.metrics.net_return_r,
        expectancy_r: segment.metrics.expectancy_r,
        profit_factor: segment.metrics.profit_factor,
        max_drawdown_r: segment.metrics.max_drawdown_r,
    }
}

fn staged_result_line(
    stage: TuningStage,
    plan: PlanKind,
    timeframe: Timeframe,
    label: &str,
    segment: &ChronologicalSegmentMetrics,
    excluded_boundary_trades: u64,
) -> String {
    let stage = match stage {
        TuningStage::Train => "TRAIN",
        TuningStage::Validation => "VALIDATION",
        TuningStage::Test => "TEST",
    };
    let metrics = stage_metrics(segment);
    format!(
        "STAGED,{stage},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        timeframe.as_str(),
        plan.as_str(),
        label,
        segment.range.start(),
        segment.range.end(),
        segment.metrics.candle_count,
        metrics.total_trades,
        metrics.win_rate,
        metrics.net_r,
        metrics.expectancy_r,
        metrics.profit_factor,
        metrics.max_drawdown_r,
        excluded_boundary_trades,
        segment.diagnostics.hard_stop_count,
        segment.diagnostics.hard_stop_frequency,
        segment.diagnostics.post_stop_mfe_r,
        segment.diagnostics.near_target_reversal_count,
        segment.diagnostics.near_target_reversal_frequency,
        segment.diagnostics.partial_fill_count,
        segment.diagnostics.partial_fill_frequency,
        segment.diagnostics.weighted_net_return_r,
        segment.diagnostics.runner_contribution_r,
        segment.diagnostics.emergency_exit_count,
        segment.diagnostics.emergency_exit_effectiveness_r,
        segment.diagnostics.suggest_close_count,
        segment.diagnostics.suggest_close_effectiveness_r,
    )
}

fn sort_candidate_results(results: &mut [CandidateResult]) {
    results.sort_by(|left, right| {
        right
            .metrics
            .net_return_r
            .cmp(&left.metrics.net_return_r)
            .then(right.metrics.expectancy_r.cmp(&left.metrics.expectancy_r))
            .then(right.metrics.total_trades.cmp(&left.metrics.total_trades))
    });
}

fn tuning_candles(count: usize, timeframe: Timeframe) -> Vec<market_domain::Candle> {
    let pattern = [
        0, 2, 4, 6, 8, 10, 8, 6, 4, 2, -1, -3, -5, -7, -9, -11, -9, -7, -5, -3,
    ];
    let symbol = Symbol::new("XAUUSDT").expect("symbol");
    let duration = timeframe.duration_ms();
    (0..count)
        .map(|index| {
            let cycle = i64::try_from(index / pattern.len()).expect("cycle");
            let wave = pattern[index % pattern.len()];
            let close_units = 400_000 + i64::from(wave * 10) + cycle;
            let open_units = close_units + if index % 2 == 0 { -5 } else { 5 };
            let high_units = open_units.max(close_units) + 20;
            let low_units = open_units.min(close_units) - 20;
            let open_time = i64::try_from(index).expect("index") * duration;
            market_domain::Candle::new(
                symbol.clone(),
                timeframe,
                open_time,
                open_time + duration - 1,
                Decimal::new(open_units, 2),
                Decimal::new(high_units, 2),
                Decimal::new(low_units, 2),
                Decimal::new(close_units, 2),
            )
            .expect("candle")
        })
        .collect()
}

fn build_replay_frames_with_higher<'a>(
    candles: &'a [market_domain::Candle],
    higher_candles: Option<&'a Vec<market_domain::Candle>>,
    warmup_bars: usize,
) -> impl Iterator<Item = ReplayFrame> + 'a {
    let mut higher_fact_cache = None::<(usize, TimeframeFacts)>;
    candles.iter().enumerate().map(move |(index, candle)| {
        let ready = index + 1 >= warmup_bars;
        let start = index.saturating_sub(499);
        let facts = if ready {
            build_ict_smc_tuning_facts(&candles[start..=index]).expect("facts")
        } else {
            Vec::new()
        };
        let higher_timeframes = higher_candles
            .and_then(|higher| {
                let end = higher
                    .partition_point(|higher_candle| higher_candle.close_time <= candle.close_time)
                    .checked_sub(1)?;
                let start = end.saturating_sub(499);
                if end + 1 < warmup_bars {
                    return None;
                }
                if let Some((cached_end, cached)) = &higher_fact_cache
                    && *cached_end == end
                {
                    return Some(cached.clone());
                }
                let higher_window = &higher[start..=end];
                let higher_facts =
                    build_ict_smc_higher_tuning_facts(higher_window).expect("higher facts");
                let latest = higher_window.last().expect("higher window");
                let facts = TimeframeFacts {
                    timeframe: latest.timeframe,
                    evaluated_closed_candle_time: latest.close_time,
                    facts: higher_facts,
                };
                higher_fact_cache = Some((end, facts.clone()));
                Some(facts)
            })
            .into_iter()
            .collect();
        ReplayFrame {
            candle: candle.clone(),
            snapshot: CanonicalMarketSnapshot {
                symbol: candle.symbol.clone(),
                target_timeframe: candle.timeframe,
                price_tick: Decimal::new(1, 2),
                evaluated_closed_candle_time: candle.close_time,
                readiness: if ready {
                    Readiness::Ready
                } else {
                    Readiness::NotReady {
                        reason: ReadinessReason::InsufficientHistory,
                    }
                },
                target: TimeframeFacts {
                    timeframe: candle.timeframe,
                    evaluated_closed_candle_time: candle.close_time,
                    facts,
                },
                higher_timeframes,
                primitive_versions: Vec::new(),
            },
            directives: Vec::new(),
            warmup: !ready,
        }
    })
}

fn tuning_warmup_bars(plan: PlanKind, timeframe: Timeframe) -> usize {
    if plan == PlanKind::Ict && matches!(timeframe, Timeframe::H4 | Timeframe::D1) {
        150
    } else {
        200
    }
}

fn latest_bias_direction(
    snapshot: &CanonicalMarketSnapshot,
    higher_timeframe: Option<Timeframe>,
) -> Option<Direction> {
    if let Some(higher_timeframe) = higher_timeframe {
        return snapshot
            .higher_timeframes
            .iter()
            .find(|facts| facts.timeframe == higher_timeframe)
            .and_then(|facts| {
                decoded_facts::<StructureEvent>(&facts.facts, FactKind::Structure)
                    .into_iter()
                    .max_by_key(|fact| fact.id.confirmed_at)
            })
            .map(|structure| structure.direction);
    }
    decoded_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)
        .into_iter()
        .max_by_key(|fact| fact.id.confirmed_at)
        .map(|structure| structure.direction)
}

fn ict_path_progress(
    snapshot: &CanonicalMarketSnapshot,
    direction: Direction,
    sweep_mode: IctSweepMode,
    fvg_mode: IctFvgMode,
) -> IctPathProgress {
    let mut progress = IctPathProgress::default();
    let Some(sweep) =
        decoded_facts::<LiquiditySweep>(&snapshot.target.facts, FactKind::LiquiditySweep)
            .into_iter()
            .filter(|fact| match sweep_mode {
                IctSweepMode::SameBias => fact.direction == direction,
                IctSweepMode::OppositeBias => {
                    fact.direction != direction && fact.direction != Direction::None
                }
            })
            .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return progress;
    };
    progress.sweep = true;

    let Some(displacement) =
        decoded_facts::<Displacement>(&snapshot.target.facts, FactKind::Displacement)
            .into_iter()
            .filter(|fact| {
                fact.direction == direction && fact.id.confirmed_at >= sweep.id.confirmed_at
            })
            .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return progress;
    };
    progress.displacement = true;

    let Some(structure) =
        decoded_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)
            .into_iter()
            .filter(|fact| {
                fact.direction == direction && fact.id.confirmed_at >= displacement.id.confirmed_at
            })
            .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return progress;
    };
    progress.structure = true;

    let expected_fvg_direction = match direction {
        Direction::Buy => FairValueGapDirection::Bullish,
        Direction::Sell => FairValueGapDirection::Bearish,
        Direction::None => return progress,
    };
    let Some(fvg) = decoded_facts::<FairValueGap>(&snapshot.target.facts, FactKind::FairValueGap)
        .into_iter()
        .filter(|fact| {
            fact.direction == expected_fvg_direction
                && fact.lifecycle != FactLifecycle::Filled
                && match fvg_mode {
                    IctFvgMode::RelatedDisplacement => {
                        fact.related_displacement_id.as_ref() == Some(&displacement.id)
                    }
                    IctFvgMode::NearestAfterDisplacement => {
                        fact.id.confirmed_at >= displacement.id.confirmed_at
                    }
                }
        })
        .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return progress;
    };
    progress.fvg = true;

    let swing_kind = match direction {
        Direction::Buy => FactKind::SwingLow,
        Direction::Sell => FactKind::SwingHigh,
        Direction::None => return progress,
    };
    let Some(invalidation_swing) =
        decoded_facts::<ConfirmedSwing>(&snapshot.target.facts, swing_kind)
            .into_iter()
            .filter(|swing| swing.confirmed_at <= structure.id.confirmed_at)
            .max_by_key(|swing| swing.confirmed_at)
    else {
        return progress;
    };
    progress.invalidation_swing = true;

    let target_side = match direction {
        Direction::Buy => LiquiditySide::BuySide,
        Direction::Sell => LiquiditySide::SellSide,
        Direction::None => return progress,
    };
    let entry = (fvg.low + fvg.high) / Decimal::TWO;
    let Some(target) = decoded_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)
        .into_iter()
        .filter(|fact| {
            fact.side == target_side
                && fact.lifecycle == FactLifecycle::Confirmed
                && match direction {
                    Direction::Buy => fact.level > entry,
                    Direction::Sell => fact.level < entry,
                    Direction::None => false,
                }
        })
        .min_by_key(|fact| (fact.level - entry).abs())
    else {
        return progress;
    };
    progress.target_liquidity = true;

    let atr = latest_decimal_fact(snapshot, FactKind::Atr).unwrap_or(Decimal::ZERO);
    if atr <= Decimal::ZERO {
        return progress;
    }
    progress.atr_positive = true;

    let buffer = atr * Decimal::new(1, 1);
    let stop_loss = match direction {
        Direction::Buy => sweep.extreme.min(invalidation_swing.price) - buffer,
        Direction::Sell => sweep.extreme.max(invalidation_swing.price) + buffer,
        Direction::None => return progress,
    };
    let risk = (entry - stop_loss).abs();
    let reward = (target.level - entry).abs();
    if risk > Decimal::ZERO {
        progress.risk_reward = Some(reward / risk);
    }
    progress
}

fn decoded_facts<T>(facts: &[ConfirmedFact<Value>], kind: FactKind) -> Vec<T>
where
    T: serde::de::DeserializeOwned,
{
    facts
        .iter()
        .filter(|fact| fact.id.kind == kind)
        .filter_map(|fact| serde_json::from_value(fact.value.clone()).ok())
        .collect()
}

fn latest_decimal_fact(snapshot: &CanonicalMarketSnapshot, kind: FactKind) -> Option<Decimal> {
    snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == kind)
        .max_by_key(|fact| fact.id.confirmed_at)
        .and_then(|fact| {
            let value = fact
                .value
                .get("value")
                .cloned()
                .unwrap_or_else(|| fact.value.clone());
            serde_json::from_value(value).ok()
        })
}

fn aggregate_metrics(
    trades: Vec<BacktestTrade>,
    candle_count: u64,
) -> backtest_engine::BacktestMetrics {
    compute_metrics(
        trades,
        0,
        current_unix_millis(),
        candle_count,
        FeeConfig::default(),
        SlippageConfig::default(),
    )
}

fn tuning_diagnostic_observations(
    candles: &[market_domain::Candle],
    trades: &[BacktestTrade],
) -> Vec<BacktestDiagnosticObservation> {
    let near_target_distance_r = Decimal::new(1, 1);

    trades
        .iter()
        .map(|trade| {
            let next_entry = trades
                .iter()
                .filter(|candidate| candidate.entry_time > trade.exit_time)
                .map(|candidate| candidate.entry_time)
                .min()
                .unwrap_or(i64::MAX);
            let risk = trade
                .initial_risk_price
                .unwrap_or_else(|| (trade.entry_price - trade.exit_price).abs());
            let post_stop_mfe_r =
                (trade.exit_reason == "STOP_LOSS" && !trade.partial_filled && !risk.is_zero())
                    .then(|| {
                        candles
                            .iter()
                            .filter(|candle| {
                                candle.open_time > trade.exit_time && candle.open_time < next_entry
                            })
                            .map(|candle| match trade.direction {
                                Direction::Buy => candle.high - trade.exit_price,
                                Direction::Sell => trade.exit_price - candle.low,
                                Direction::None => Decimal::ZERO,
                            })
                            .fold(Decimal::ZERO, Decimal::max)
                            / risk
                    });
            let near_target_reversal = trade.tp2_execution_edge_r.is_some_and(|edge_r| {
                let approach_r = (edge_r - near_target_distance_r).max(Decimal::ZERO);
                trade.exit_reason != "TAKE_PROFIT"
                    && trade.mfe_r >= approach_r
                    && trade.mfe_r < edge_r
            });
            BacktestDiagnosticObservation {
                trade_id: trade.trade_id,
                post_stop_mfe_r,
                near_target_reversal,
            }
        })
        .collect()
}

struct ChronologicalSegmentMetrics {
    range: std::ops::RangeInclusive<i64>,
    metrics: backtest_engine::BacktestMetrics,
    diagnostics: backtest_engine::BacktestDiagnostics,
}

struct ChronologicalTuningMetrics {
    train: ChronologicalSegmentMetrics,
    validation: ChronologicalSegmentMetrics,
    test: ChronologicalSegmentMetrics,
    excluded_boundary_trades: u64,
}

fn chronological_tuning_metrics(
    candles: &[market_domain::Candle],
    trades: &[BacktestTrade],
) -> Result<ChronologicalTuningMetrics, &'static str> {
    if candles.len() < 5 {
        return Err("chronological tuning requires at least five candles");
    }
    let train_end = candles.len() * 60 / 100;
    let validation_end = candles.len() * 80 / 100;
    if train_end == 0 || validation_end <= train_end || validation_end >= candles.len() {
        return Err("chronological tuning split is empty");
    }

    let ranges = [
        (0, train_end),
        (train_end, validation_end),
        (validation_end, candles.len()),
    ];
    let mut assigned = 0_u64;
    let mut segments = Vec::with_capacity(ranges.len());
    for (start, end_exclusive) in ranges {
        let start_time = candles[start].open_time;
        let end_time = candles[end_exclusive - 1].close_time;
        let segment_trades = trades
            .iter()
            .filter(|trade| trade.entry_time >= start_time && trade.exit_time <= end_time)
            .cloned()
            .collect::<Vec<_>>();
        assigned += segment_trades.len() as u64;
        let observations =
            tuning_diagnostic_observations(&candles[start..end_exclusive], &segment_trades);
        let diagnostics = compute_diagnostics(&segment_trades, &observations);
        segments.push(ChronologicalSegmentMetrics {
            range: start_time..=end_time,
            metrics: compute_metrics(
                segment_trades,
                start_time,
                end_time,
                (end_exclusive - start) as u64,
                FeeConfig::default(),
                SlippageConfig::default(),
            ),
            diagnostics,
        });
    }
    let mut segments = segments.into_iter();
    Ok(ChronologicalTuningMetrics {
        train: segments.next().expect("train segment"),
        validation: segments.next().expect("validation segment"),
        test: segments.next().expect("test segment"),
        excluded_boundary_trades: trades.len() as u64 - assigned,
    })
}

fn htf_metric_counts(trades: &[BacktestTrade]) -> HtfMetricCounts {
    let mut counts = HtfMetricCounts::default();
    for trade in trades {
        match trade
            .htf_context
            .as_ref()
            .map(|context| context.alignment)
            .unwrap_or(analysis_domain::HtfAlignment::Missing)
        {
            analysis_domain::HtfAlignment::Aligned => counts.aligned += 1,
            analysis_domain::HtfAlignment::Counter => counts.counter += 1,
            analysis_domain::HtfAlignment::Neutral => counts.neutral += 1,
            analysis_domain::HtfAlignment::Missing => counts.missing += 1,
            analysis_domain::HtfAlignment::Off => counts.off += 1,
        }
    }
    counts
}

fn tuning_filter_from_env() -> TuningFilter {
    tuning_filter_from_values(
        std::env::var("GOLD_TUNING_PLAN").ok().as_deref(),
        std::env::var("GOLD_TUNING_TIMEFRAME").ok().as_deref(),
        std::env::var("GOLD_TUNING_CANDIDATE").ok().as_deref(),
    )
}

fn selected_tuning_candidate(
    plan: PlanKind,
    timeframe: Timeframe,
    label: &str,
) -> Result<Candidate, String> {
    if label.eq_ignore_ascii_case("baseline-current") {
        return Ok(Candidate {
            label: "baseline-current".to_owned(),
            parameters: default_parameters(plan, timeframe),
        });
    }
    let candidates = match plan {
        PlanKind::Ict => ict_candidates(timeframe),
        PlanKind::Smc => smc_candidates(timeframe),
        PlanKind::Rsi => rsi_candidates(),
        PlanKind::Quasimodo => qm_candidates(),
        PlanKind::TrendBreakout => trend_breakout_candidates(timeframe),
        PlanKind::MeanReversion => mean_reversion_candidates(),
    };
    candidates
        .into_iter()
        .find(|candidate| candidate.label.eq_ignore_ascii_case(label))
        .ok_or_else(|| {
            format!(
                "unknown tuning candidate plan={} timeframe={} label={label}",
                plan.as_str(),
                timeframe.as_str()
            )
        })
}

fn all_tuning_candidates(plan: PlanKind, timeframe: Timeframe) -> Vec<Candidate> {
    match plan {
        PlanKind::Ict => ict_candidates(timeframe),
        PlanKind::Smc => smc_candidates(timeframe),
        PlanKind::Rsi => rsi_candidates(),
        PlanKind::Quasimodo => qm_candidates(),
        PlanKind::TrendBreakout => trend_breakout_candidates(timeframe),
        PlanKind::MeanReversion => mean_reversion_candidates(),
    }
}

fn staged_candidates_from_values(
    plan: PlanKind,
    timeframe: Timeframe,
    stage: TuningStage,
    shortlist: Option<&str>,
    winner: Option<&str>,
) -> Result<Vec<Candidate>, String> {
    match stage {
        TuningStage::Train => Ok(all_tuning_candidates(plan, timeframe)),
        TuningStage::Validation => {
            let labels =
                shortlist.ok_or_else(|| "validation requires GOLD_TUNING_CANDIDATES".to_owned())?;
            labels
                .split(',')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(|label| selected_tuning_candidate(plan, timeframe, label))
                .collect()
        }
        TuningStage::Test => {
            let winner = winner.ok_or_else(|| "test requires GOLD_TUNING_CANDIDATE".to_owned())?;
            Ok(vec![
                selected_tuning_candidate(plan, timeframe, "baseline-current")?,
                selected_tuning_candidate(plan, timeframe, winner)?,
            ])
        }
    }
}

fn staged_replay_end(stage: TuningStage, split: &ChronologicalBoundaries) -> usize {
    match stage {
        TuningStage::Train => split.train.end,
        TuningStage::Validation => split.validation.end,
        TuningStage::Test => split.test.end,
    }
}

fn validation_candidate_labels(candidate: &str, include_baseline: bool) -> Vec<&str> {
    if include_baseline && !candidate.eq_ignore_ascii_case("baseline-current") {
        vec!["baseline-current", candidate]
    } else {
        vec![candidate]
    }
}

fn tuning_filter_from_values(
    plan: Option<&str>,
    timeframe: Option<&str>,
    candidate: Option<&str>,
) -> TuningFilter {
    TuningFilter {
        plan: plan.and_then(parse_tuning_plan),
        timeframe: timeframe.and_then(parse_tuning_timeframe),
        candidate: candidate
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty()),
    }
}

fn ict_smc_tuning_load_timeframes(filter: &TuningFilter) -> std::collections::BTreeSet<Timeframe> {
    let mut timeframes = std::collections::BTreeSet::new();
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        timeframes.insert(timeframe);
        if let Some(higher) = next_higher_timeframe(timeframe) {
            timeframes.insert(higher);
        }
    }
    timeframes
}

fn trend_mean_tuning_load_timeframes(
    filter: &TuningFilter,
) -> std::collections::BTreeSet<Timeframe> {
    let mut timeframes = std::collections::BTreeSet::new();
    let needs_trend_higher = filter.includes_plan(PlanKind::TrendBreakout);
    for timeframe in Timeframe::ALL {
        if !filter.includes_timeframe(timeframe) {
            continue;
        }
        timeframes.insert(timeframe);
        if needs_trend_higher && let Some(higher) = next_higher_timeframe(timeframe) {
            timeframes.insert(higher);
        }
    }
    timeframes
}

fn parse_tuning_plan(value: &str) -> Option<PlanKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ict" => Some(PlanKind::Ict),
        "smc" => Some(PlanKind::Smc),
        "rsi" => Some(PlanKind::Rsi),
        "qm" | "quasimodo" => Some(PlanKind::Quasimodo),
        "trend" | "trend_breakout" | "trend-breakout" => Some(PlanKind::TrendBreakout),
        "mean" | "mean_reversion" | "mean-reversion" => Some(PlanKind::MeanReversion),
        _ => None,
    }
}

fn parse_tuning_timeframe(value: &str) -> Option<Timeframe> {
    value.trim().parse::<Timeframe>().ok()
}

fn backtest_trade(sequence: i64, plan: PlanKind, timeframe: Timeframe) -> BacktestTrade {
    BacktestTrade {
        trade_id: uuid::Uuid::from_u128(100_000 + sequence as u128),
        signal_id: uuid::Uuid::from_u128(200_000 + sequence as u128),
        sequence,
        symbol: Symbol::new("XAUUSDT").expect("symbol"),
        plan,
        timeframe,
        direction: Direction::Buy,
        htf_context: None,
        entry_time: sequence * 60_000,
        exit_time: sequence * 60_000 + 60_000,
        entry_price: Decimal::new(100, 0),
        exit_price: Decimal::new(110, 0),
        gross_r: Decimal::ONE,
        net_r: Decimal::ONE,
        mfe_r: Decimal::ONE,
        mae_r: Decimal::ZERO,
        duration_ms: 60_000,
        exit_reason: "TAKE_PROFIT".to_owned(),
        fees_r: Decimal::ZERO,
        slippage_r: Decimal::ZERO,
        ambiguous_intrabar: false,
        partial_filled: false,
        realized_r: Decimal::ZERO,
        runner_contribution_r: None,
        hard_stop_counterfactual_r: None,
        tp2_execution_edge_r: None,
        initial_risk_price: None,
    }
}

fn rsi_v5_candidates(base: &RsiPlanParameters) -> Vec<Candidate> {
    (1..=3)
        .map(|reversal_confirmation_bars| {
            let mut parameters = base.clone();
            parameters.reversal_confirmation_bars = reversal_confirmation_bars;
            Candidate {
                label: format!("rsi-v5-confirm{reversal_confirmation_bars}"),
                parameters: PlanParameters::Rsi(parameters),
            }
        })
        .collect()
}

fn rsi_candidates() -> Vec<Candidate> {
    [
        (30, 70, false, Decimal::new(2, 0), Decimal::new(2, 1)),
        (30, 70, false, Decimal::new(15, 1), Decimal::new(2, 1)),
        (30, 70, false, Decimal::new(15, 1), Decimal::new(35, 2)),
        (35, 65, false, Decimal::new(15, 1), Decimal::new(2, 1)),
        (35, 65, false, Decimal::new(125, 2), Decimal::new(35, 2)),
        (30, 70, true, Decimal::new(15, 1), Decimal::new(35, 2)),
        (35, 65, true, Decimal::new(15, 1), Decimal::new(35, 2)),
        (25, 75, false, Decimal::new(15, 1), Decimal::new(2, 1)),
    ]
    .into_iter()
    .map(
        |(oversold, overbought, divergence_required, target_rr, atr_buffer_multiplier)| Candidate {
            label: format!(
                "rsi-os{}-ob{}-div{}-rr{}-atr{}",
                oversold, overbought, divergence_required, target_rr, atr_buffer_multiplier
            ),
            parameters: PlanParameters::Rsi(RsiPlanParameters {
                rsi_period: 14,
                oversold_threshold: Decimal::new(oversold, 0),
                overbought_threshold: Decimal::new(overbought, 0),
                divergence_required,
                minimum_pivot_distance: 2,
                maximum_pivot_distance: 20,
                confirmed_swing_lookback: 50,
                atr_period: 14,
                atr_buffer_multiplier,
                target_rr,
                max_bars_valid: 10,
                reversal_confirmation_bars: 2,
            }),
        },
    )
    .collect()
}

fn ict_candidates(timeframe: Timeframe) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for fvg_selection in [
        IctFvgSelection::RelatedDisplacement,
        IctFvgSelection::NearestAfterDisplacement,
    ] {
        for sweep_bias_mode in [IctSweepBiasMode::SameBias, IctSweepBiasMode::OppositeBias] {
            for minimum_rr in [
                Decimal::new(15, 1),
                Decimal::new(12, 1),
                Decimal::new(1, 0),
                Decimal::new(8, 1),
            ] {
                for atr_buffer_multiplier in [Decimal::new(2, 1), Decimal::new(1, 1)] {
                    for entry_max_bars_valid in [10, 20] {
                        let label = format!(
                            "ict-fvg{}-sweep{}-rr{}-atr{}-entry{}",
                            ict_fvg_label(fvg_selection),
                            ict_sweep_label(sweep_bias_mode),
                            minimum_rr,
                            atr_buffer_multiplier,
                            entry_max_bars_valid
                        );
                        candidates.push(Candidate {
                            label,
                            parameters: PlanParameters::Ict(IctParameters {
                                higher_timeframe: next_higher_timeframe(timeframe),
                                fvg_selection,
                                sweep_bias_mode,
                                atr_period: 14,
                                atr_buffer_multiplier,
                                minimum_rr,
                                bias_max_bars_valid: 20,
                                sweep_max_bars_valid: 10,
                                displacement_max_bars_valid: 5,
                                entry_max_bars_valid,
                            }),
                        });
                    }
                }
            }
        }
    }
    candidates
}

fn ict_fvg_label(selection: IctFvgSelection) -> &'static str {
    match selection {
        IctFvgSelection::RelatedDisplacement => "related",
        IctFvgSelection::NearestAfterDisplacement => "nearest",
    }
}

fn ict_sweep_label(mode: IctSweepBiasMode) -> &'static str {
    match mode {
        IctSweepBiasMode::SameBias => "same",
        IctSweepBiasMode::OppositeBias => "opposite",
    }
}

fn smc_candidates(timeframe: Timeframe) -> Vec<Candidate> {
    let mut modes = vec![
        default_smc_htf_mode(timeframe),
        HtfFilterMode::Weighted,
        HtfFilterMode::Off,
    ];
    modes.dedup();
    let mut candidates = Vec::new();
    for htf_mode in modes {
        for minimum_rr in [Decimal::new(15, 1), Decimal::new(12, 1), Decimal::new(1, 0)] {
            for atr_buffer_multiplier in [Decimal::new(2, 1), Decimal::new(1, 1)] {
                let label = format!(
                    "smc-htf{}-rr{}-atr{}",
                    htf_mode_label(htf_mode),
                    minimum_rr,
                    atr_buffer_multiplier
                );
                candidates.push(Candidate {
                    label,
                    parameters: PlanParameters::Smc(SmcParameters {
                        higher_timeframe: next_higher_timeframe(timeframe),
                        htf_mode,
                        weighted_countertrend_penalty: Decimal::new(2, 1),
                        atr_period: 14,
                        atr_buffer_multiplier,
                        minimum_rr,
                        max_bars_valid: 10,
                    }),
                });
            }
        }
    }
    candidates
}

fn default_smc_htf_mode(timeframe: Timeframe) -> HtfFilterMode {
    if matches!(
        timeframe,
        Timeframe::M1 | Timeframe::M5 | Timeframe::M15 | Timeframe::M30
    ) {
        HtfFilterMode::Strict
    } else {
        HtfFilterMode::Weighted
    }
}

fn htf_mode_label(mode: HtfFilterMode) -> &'static str {
    match mode {
        HtfFilterMode::Strict => "strict",
        HtfFilterMode::Weighted => "weighted",
        HtfFilterMode::Off => "off",
    }
}

fn trend_breakout_candidates(timeframe: Timeframe) -> Vec<Candidate> {
    let mut htf_options = next_higher_timeframe(timeframe)
        .map(|higher| vec![("on", Some(higher)), ("off", None)])
        .unwrap_or_else(|| vec![("off", None)]);
    htf_options.dedup();
    let mut candidates = Vec::new();
    for (htf_label, higher_timeframe) in htf_options {
        for minimum_adx in [Decimal::new(20, 0), Decimal::new(25, 0)] {
            for initial_stop_atr_multiplier in [Decimal::new(15, 1), Decimal::new(2, 0)] {
                for target_rr in [Decimal::new(15, 1), Decimal::new(2, 0)] {
                    for trailing_activation_r in [Decimal::new(75, 2), Decimal::new(1, 0)] {
                        let label = format!(
                            "trend-htf{}-adx{}-stop{}-rr{}-trail{}",
                            htf_label,
                            minimum_adx,
                            initial_stop_atr_multiplier,
                            target_rr,
                            trailing_activation_r
                        );
                        candidates.push(Candidate {
                            label,
                            parameters: PlanParameters::TrendBreakout(TrendBreakoutParameters {
                                donchian_period: 20,
                                ema_period: 50,
                                adx_period: 14,
                                minimum_adx,
                                higher_timeframe,
                                atr_period: 14,
                                initial_stop_atr_multiplier,
                                target_rr,
                                trailing_activation_r,
                                chandelier_period: 22,
                                chandelier_atr_multiplier: Decimal::new(3, 0),
                            }),
                        });
                    }
                }
            }
        }
    }
    candidates
}

fn mean_reversion_candidates() -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for extreme_zscore in [Decimal::new(15, 1), Decimal::new(2, 0)] {
        for maximum_adx in [Decimal::new(20, 0), Decimal::new(25, 0)] {
            for maximum_bandwidth in [Decimal::new(5, 2), Decimal::new(8, 2)] {
                for minimum_rr in [Decimal::new(8, 1), Decimal::new(1, 0), Decimal::new(12, 1)] {
                    for atr_buffer_multiplier in
                        [Decimal::new(5, 2), Decimal::new(1, 1), Decimal::new(2, 1)]
                    {
                        let label = format!(
                            "mean-z{}-adx{}-bw{}-rr{}-atr{}",
                            extreme_zscore,
                            maximum_adx,
                            maximum_bandwidth,
                            minimum_rr,
                            atr_buffer_multiplier
                        );
                        candidates.push(Candidate {
                            label,
                            parameters: PlanParameters::MeanReversion(MeanReversionParameters {
                                bollinger_period: 20,
                                bollinger_deviation: Decimal::new(2, 0),
                                zscore_period: 20,
                                extreme_zscore,
                                adx_period: 14,
                                maximum_adx,
                                maximum_bandwidth,
                                atr_period: 14,
                                atr_buffer_multiplier,
                                minimum_rr,
                                max_bars_valid: 10,
                            }),
                        });
                    }
                }
            }
        }
    }
    candidates
}

fn qm_candidates() -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for minimum_zone_ticks in [5, 10] {
        for tolerance_atr_multiplier in
            [Decimal::new(2, 1), Decimal::new(35, 2), Decimal::new(5, 1)]
        {
            for atr_buffer_multiplier in [Decimal::new(2, 1), Decimal::new(35, 2)] {
                for minimum_rr in [Decimal::new(15, 1)] {
                    for max_structure_age_bars in [5, 10] {
                        for max_entry_distance_atr in [Decimal::new(3, 0), Decimal::new(5, 0)] {
                            let label = format!(
                                "qm-zone{}-tol{}-buf{}-rr{}-age{}-dist{}",
                                minimum_zone_ticks,
                                tolerance_atr_multiplier,
                                atr_buffer_multiplier,
                                minimum_rr,
                                max_structure_age_bars,
                                max_entry_distance_atr
                            );
                            candidates.push(Candidate {
                                label,
                                parameters: PlanParameters::Quasimodo(QuasimodoParameters {
                                    minimum_zone_ticks,
                                    tolerance_atr_multiplier,
                                    atr_period: 14,
                                    atr_buffer_multiplier,
                                    minimum_rr,
                                    max_bars_valid: 10,
                                    max_structure_age_bars,
                                    max_entry_distance_atr,
                                }),
                            });
                        }
                    }
                }
            }
        }
    }
    candidates
}
