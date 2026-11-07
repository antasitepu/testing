use analysis_domain::{
    AnalysisWorkItem, Direction, EvaluationIdentity, ExecutionInstruction, IctParameters, PlanKind,
    PlanParameters, PlanProfileKey, PriceZone, ReleaseMode, SignalDefinition, SignalEvent,
    SignalEventKind, StructuredAnalysisError, VersionIdentity, WorkSource, WorkStatus,
};
use analysis_service::{
    AnalysisWorkStore, BacktestRunInput, BacktestRunRecord, BacktestStore, ConfigurationRelease,
    ConfigurationStore, EvaluationCommit, MaterializedProfile, MonitoringEvent, ReleaseLifecycle,
    SignalHistoryQuery, SignalStore, ValidationGate,
};
use analysis_storage::{
    BacktestRepository, ConfigurationRepository, MIGRATOR, SignalRepository, WorkRepository,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const NOW_MS: i64 = 1_782_195_600_000;

async fn test_pool() -> PgPool {
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
         rejected_plan_candidates, trade_management_directives, signal_events, signals, \
         analysis_evaluations, plan_runtime_states, analysis_work_items, \
         analysis_runtime_state, analysis_profiles, configuration_releases, \
         market_sessions, sync_states, candles, markets RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate phase 1 and phase 2 tables");
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .expect("seed market");
    pool
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn profile(release_id: Uuid, plan: PlanKind, timeframe: Timeframe) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan,
            target_timeframe: timeframe,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters {
            reversal_giveback_r: Decimal::new(7, 1),
            ..analysis_domain::ProfitProtectionParameters::default()
        },
        position_management: analysis_domain::PositionManagementParameters {
            zone_buffer_multiplier: Decimal::new(7, 1),
            minimum_buffer_ticks: 12,
            target_buffer_atr_multiplier: Decimal::new(2, 1),
            target_buffer_minimum_ticks: 8,
            tp1_r: Decimal::new(12, 1),
            tp1_fraction: Decimal::new(4, 1),
            weakening_giveback_r: Decimal::new(3, 1),
        },
        versions: VersionIdentity::new(
            release_id,
            "analysis-v1",
            format!("{release_id}-parameters"),
        )
        .unwrap(),
        parameters: PlanParameters::Ict(IctParameters {
            higher_timeframe: Some(Timeframe::M5),
            fvg_selection: analysis_domain::IctFvgSelection::RelatedDisplacement,
            sweep_bias_mode: analysis_domain::IctSweepBiasMode::SameBias,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            bias_max_bars_valid: 20,
            sweep_max_bars_valid: 10,
            displacement_max_bars_valid: 5,
            entry_max_bars_valid: 10,
        }),
        required_timeframes: vec![timeframe],
        minimum_history_bars: 100,
        warmup_bars: 20,
    }
}

fn release(release_id: Uuid) -> ConfigurationRelease {
    ConfigurationRelease {
        release_id,
        symbol: symbol(),
        mode: ReleaseMode::Research,
        lifecycle: ReleaseLifecycle::Draft,
        validation_summary: json!({}),
        profiles: vec![profile(release_id, PlanKind::Ict, Timeframe::M1)],
    }
}

fn research_gate() -> ValidationGate {
    ValidationGate {
        metrics: json!({"research": true}),
        ..ValidationGate::default()
    }
}

fn work(work_id: Uuid, release_id: Uuid, close_time: i64) -> AnalysisWorkItem {
    AnalysisWorkItem {
        work_id,
        symbol: symbol(),
        timeframe: Timeframe::M1,
        canonical_close_time: close_time,
        release_id,
        source: WorkSource::Live,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: NOW_MS,
        completed_at: None,
    }
}

fn signal(signal_id: Uuid, release_id: Uuid, evaluated_at: i64) -> SignalDefinition {
    SignalDefinition {
        signal_id,
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(
            PriceZone::new(Decimal::new(410_000, 2), Decimal::new(410_100, 2)).unwrap(),
        ),
        projected_entry: None,
        stop_loss: Decimal::new(409_000, 2),
        primary_target: Decimal::new(412_000, 2),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::new(2, 0),
        pre_entry_validation: None,
        confidence: Decimal::new(75, 2),
        evidence: Vec::new(),
        effective_at: evaluated_at + 1,
        detected_at: evaluated_at + 2,
        published_at: evaluated_at + 3,
        release_id,
        algorithm_version: "analysis-v1".to_owned(),
        parameter_version: format!("{release_id}-parameters"),
        created_at: evaluated_at,
        expires_at: None,
        htf_context: None,
    }
}

fn evaluation(release_id: Uuid, close_time: i64) -> EvaluationIdentity {
    EvaluationIdentity {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        release_id,
        algorithm_version: "analysis-v1".to_owned(),
        parameter_version: format!("{release_id}-parameters"),
        evaluated_closed_candle_time: close_time,
    }
}

fn signal_event(signal_id: Uuid, sequence: i64, kind: SignalEventKind) -> SignalEvent {
    SignalEvent {
        event_id: Uuid::now_v7(),
        signal_id,
        sequence,
        kind,
        effective_at: NOW_MS + sequence + 1,
        recorded_at: NOW_MS + sequence + 2,
        virtual_trade_id: None,
        price: None,
        reason: None,
    }
}

#[tokio::test]
async fn postgres_contract_preserves_immutability_idempotency_and_atomicity() {
    let pool = test_pool().await;
    let configurations = ConfigurationRepository::new(pool.clone());
    let work_items = WorkRepository::new(pool.clone());
    let backtests = BacktestRepository::new(pool.clone());
    let signals = SignalRepository::new(pool.clone());

    let first_release_id = Uuid::now_v7();
    configurations
        .create_draft(&release(first_release_id))
        .await
        .unwrap();
    sqlx::query("UPDATE analysis_profiles SET parameters = $2 WHERE release_id = $1")
        .bind(first_release_id)
        .bind(json!({"explicitFixture": true, "draftEdit": true}))
        .execute(&pool)
        .await
        .expect("draft profile remains editable");
    configurations
        .activate(first_release_id, &research_gate())
        .await
        .unwrap();

    let profile_update =
        sqlx::query("UPDATE analysis_profiles SET enabled = FALSE WHERE release_id = $1")
            .bind(first_release_id)
            .execute(&pool)
            .await;
    assert!(
        profile_update.is_err(),
        "materialized profiles are immutable"
    );

    let second_release_id = Uuid::now_v7();
    configurations
        .create_draft(&release(second_release_id))
        .await
        .unwrap();
    configurations
        .activate(second_release_id, &research_gate())
        .await
        .unwrap();
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM configuration_releases \
         WHERE symbol = 'XAUUSDT' AND lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_count, 1);
    let active_profile = &configurations.active_profiles(&symbol()).await.unwrap()[0];
    assert_eq!(active_profile.key.target_timeframe, Timeframe::M1);
    let stored_position_management: serde_json::Value = sqlx::query_scalar(
        "SELECT position_management FROM analysis_profiles WHERE release_id = $1",
    )
    .bind(second_release_id)
    .fetch_one(&pool)
    .await
    .expect("position_management column is persisted");
    assert_eq!(
        stored_position_management,
        serde_json::to_value(&active_profile.position_management).unwrap()
    );
    assert_eq!(
        active_profile.profit_protection.reversal_giveback_r,
        Decimal::new(7, 1)
    );
    assert_eq!(
        active_profile.position_management,
        release(second_release_id).profiles[0].position_management
    );
    let empty_history = signals
        .history(SignalHistoryQuery {
            key: PlanProfileKey {
                symbol: symbol(),
                plan: PlanKind::Ict,
                target_timeframe: Timeframe::M1,
            },
            before_created_at: None,
            limit: 50,
        })
        .await
        .unwrap();
    assert!(empty_history.items.is_empty());

    let lease_work_id = Uuid::now_v7();
    work_items
        .enqueue(&work(lease_work_id, second_release_id, NOW_MS))
        .await
        .unwrap();
    work_items
        .enqueue(&work(Uuid::now_v7(), second_release_id, NOW_MS))
        .await
        .unwrap();
    let work_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analysis_work_items WHERE close_time = $1")
            .bind(NOW_MS)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(work_count, 1, "duplicate work identity is deduplicated");

    let first_lease = work_items
        .lease_next("worker-a", NOW_MS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_lease.work_id, lease_work_id);
    let reclaimed = work_items
        .lease_next("worker-b", NOW_MS + 60_001)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.work_id, lease_work_id);
    assert_eq!(reclaimed.lease_owner.as_deref(), Some("worker-b"));
    assert_eq!(reclaimed.attempt_count, 2);
    // The stale lease holder is fenced out once the item was re-leased.
    assert!(
        work_items
            .defer(lease_work_id, first_lease.attempt_count)
            .await
            .is_err()
    );
    assert!(
        work_items
            .complete_evaluation(EvaluationCommit {
                work_id: lease_work_id,
                lease_attempt: first_lease.attempt_count,
                ..EvaluationCommit::default()
            })
            .await
            .is_err()
    );

    let rollback_signal_id = Uuid::now_v7();
    let rollback_commit = EvaluationCommit {
        work_id: lease_work_id,
        lease_attempt: reclaimed.attempt_count,
        evaluations: vec![evaluation(second_release_id, NOW_MS)],
        signals: vec![signal(rollback_signal_id, second_release_id, NOW_MS)],
        monitoring_events: vec![MonitoringEvent {
            event_id: Uuid::now_v7(),
            event_type: "SIGNAL_CREATED".to_owned(),
            entity_type: "SIGNAL".to_owned(),
            entity_id: Some(rollback_signal_id),
            payload: json!({}),
        }],
        inject_failure_after_writes: true,
        ..EvaluationCommit::default()
    };
    assert!(
        work_items
            .complete_evaluation(rollback_commit)
            .await
            .is_err()
    );
    let evaluation_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analysis_evaluations")
        .fetch_one(&pool)
        .await
        .unwrap();
    let signal_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signals")
        .fetch_one(&pool)
        .await
        .unwrap();
    let monitoring_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM monitoring_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(evaluation_count, 0, "evaluations must roll back");
    assert_eq!(signal_count, 0, "signals must roll back");
    assert_eq!(monitoring_count, 0, "monitoring outbox must roll back");
    let status: String =
        sqlx::query_scalar("SELECT status FROM analysis_work_items WHERE work_id = $1")
            .bind(lease_work_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "LEASED");

    let committed_signal_id = Uuid::now_v7();
    work_items
        .complete_evaluation(EvaluationCommit {
            work_id: lease_work_id,
            lease_attempt: reclaimed.attempt_count,
            evaluations: vec![evaluation(second_release_id, NOW_MS)],
            signals: vec![signal(committed_signal_id, second_release_id, NOW_MS)],
            signal_events: vec![signal_event(
                committed_signal_id,
                0,
                SignalEventKind::EntryAvailable,
            )],
            ..EvaluationCommit::default()
        })
        .await
        .unwrap();
    let signal_update = sqlx::query("UPDATE signals SET plan = 'SMC' WHERE signal_id = $1")
        .bind(committed_signal_id)
        .execute(&pool)
        .await;
    assert!(signal_update.is_err(), "signal definitions are immutable");

    let duplicate_event_work_id = Uuid::now_v7();
    work_items
        .enqueue(&work(
            duplicate_event_work_id,
            second_release_id,
            NOW_MS + 120_000,
        ))
        .await
        .unwrap();
    let event_lease = work_items
        .lease_next("worker-event-idempotency", NOW_MS + 120_000)
        .await
        .unwrap()
        .unwrap();
    work_items
        .complete_evaluation(EvaluationCommit {
            work_id: duplicate_event_work_id,
            lease_attempt: event_lease.attempt_count,
            evaluations: vec![evaluation(second_release_id, NOW_MS + 120_000)],
            signal_events: vec![signal_event(
                committed_signal_id,
                0,
                SignalEventKind::EntryAvailable,
            )],
            ..EvaluationCommit::default()
        })
        .await
        .unwrap();
    let signal_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_events WHERE signal_id = $1 AND sequence = 0",
    )
    .bind(committed_signal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        signal_event_count, 1,
        "duplicate signal events are idempotent"
    );

    let duplicate_work_id = Uuid::now_v7();
    work_items
        .enqueue(&work(duplicate_work_id, second_release_id, NOW_MS + 60_000))
        .await
        .unwrap();
    let duplicate_lease = work_items
        .lease_next("worker-c", NOW_MS + 60_000)
        .await
        .unwrap()
        .unwrap();
    let duplicate = work_items
        .complete_evaluation(EvaluationCommit {
            work_id: duplicate_work_id,
            lease_attempt: duplicate_lease.attempt_count,
            evaluations: vec![evaluation(second_release_id, NOW_MS)],
            ..EvaluationCommit::default()
        })
        .await;
    assert!(duplicate.is_err(), "evaluation identity must be unique");

    let run_id = Uuid::now_v7();
    backtests
        .create_run(
            BacktestRunRecord::new(
                run_id,
                BacktestRunInput {
                    symbol: symbol(),
                    plan: PlanKind::TrendBreakout,
                    target_timeframe: Timeframe::M1,
                    start_time: NOW_MS,
                    end_time: NOW_MS + 60_000,
                    release_id: first_release_id,
                    algorithm_version: "trend-breakout-v1".to_owned(),
                    parameter_version: "trend-breakout-1m-v1".to_owned(),
                    fees: analysis_service::FeeConfig::default(),
                    slippage: analysis_service::SlippageConfig::default(),
                    account: None,
                    code_commit_hash: "test-commit".to_owned(),
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    backtests
        .fail_run(
            run_id,
            StructuredAnalysisError {
                code: "fixture_failure".to_owned(),
                message: "expected".to_owned(),
                retryable: false,
            },
        )
        .await
        .unwrap();
    let (run_status, metrics): (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT status, metrics FROM backtest_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(run_status, "FAILED");
    assert!(metrics.is_none());
    let trade_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM backtest_trades WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(trade_count, 0);

    pool.close().await;
}
