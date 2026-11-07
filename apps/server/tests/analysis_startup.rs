use analysis_domain::{
    AnalysisWorkItem, Direction, DirectiveKind, ExecutionInstruction, FactKind,
    ManagementDirective, PlanKind, PlanParameters, PlanProfileKey, PriceZone, Readiness,
    ReleaseMode, SignalDefinition, TrendBreakoutParameters, VersionIdentity, WorkSource,
    WorkStatus,
};
use analysis_service::{AnalysisCandleSource, SnapshotAvailability};
use analysis_service::{
    ConfigurationRelease, ConfigurationStore, MaterializedProfile, ReleaseLifecycle, ValidationGate,
};
use analysis_storage::{ConfigurationRepository, MIGRATOR, WorkRepository};
use axum::{
    Json, Router,
    extract::OriginalUri,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use gold_monitoring_server::{config::AppConfig, runtime, session_history::SessionHistoryConfig};
use market_domain::{Candle, ClosedCandleSource, ClosedCandleWrite, Symbol, Timeframe};
use market_storage::CandleRepository;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgPoolOptions};
use std::{path::PathBuf, sync::Arc, sync::OnceLock, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{OwnedMutexGuard, oneshot},
    task::JoinHandle,
    time::timeout,
};
use url::Url;
use uuid::Uuid;

const NOW_MS: i64 = 1_782_195_600_000;
static TEST_DB_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

struct FixtureServer {
    base_url: Url,
    task: JoinHandle<()>,
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture_handler(OriginalUri(uri): OriginalUri) -> Response {
    let body: Value = match uri.path() {
        "/fapi/v1/time" => json!({"serverTime": 1782151404000_i64}),
        "/fapi/v1/exchangeInfo" => serde_json::from_str(include_str!(
            "../../../crates/binance-adapter/tests/fixtures/exchange_info.json"
        ))
        .unwrap(),
        "/fapi/v1/klines" => serde_json::from_str(include_str!(
            "../../../crates/binance-adapter/tests/fixtures/klines.json"
        ))
        .unwrap(),
        "/fapi/v1/tradingSchedule" => serde_json::from_str(include_str!(
            "../../../crates/binance-adapter/tests/fixtures/trading_schedule.json"
        ))
        .unwrap(),
        _ => return (StatusCode::NOT_FOUND, "fixture not found").into_response(),
    };
    Json(body).into_response()
}

async fn spawn_fixture_server() -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(get(fixture_handler)))
            .await
            .unwrap();
    });
    FixtureServer {
        base_url: Url::parse(&format!("http://{address}/")).unwrap(),
        task,
    }
}

async fn unused_loopback_address() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn profile(release_id: Uuid) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::TrendBreakout,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(release_id, "trend-v1", "trend-1m-v1").unwrap(),
        parameters: PlanParameters::TrendBreakout(TrendBreakoutParameters {
            donchian_period: 20,
            ema_period: 50,
            adx_period: 14,
            minimum_adx: Decimal::new(25, 0),
            higher_timeframe: None,
            atr_period: 14,
            initial_stop_atr_multiplier: Decimal::TWO,
            target_rr: Decimal::TWO,
            trailing_activation_r: Decimal::ONE,
            chandelier_period: 22,
            chandelier_atr_multiplier: Decimal::new(3, 0),
        }),
        required_timeframes: Vec::new(),
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

async fn test_db_guard() -> OwnedMutexGuard<()> {
    TEST_DB_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn test_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database")
}

fn candle(open_time: i64, step: i64) -> Candle {
    let base_cents = 410_000 + step;
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + Timeframe::M1.duration_ms() - 1,
        Decimal::new(base_cents, 2),
        Decimal::new(base_cents + 350, 2),
        Decimal::new(base_cents - 250, 2),
        Decimal::new(base_cents + 150, 2),
    )
    .unwrap()
}

#[test]
fn build_facts_emits_latest_market_close_fact() {
    let candles: Vec<_> = (0..80)
        .map(|index| {
            candle(
                1_782_000_000_000 + index * Timeframe::M1.duration_ms(),
                index,
            )
        })
        .collect();
    let facts = runtime::build_facts(&candles).unwrap();
    let close_fact = facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::MarketClose)
        .max_by_key(|fact| fact.id.confirmed_at)
        .unwrap();
    let close: Decimal = serde_json::from_value(close_fact.value["value"].clone()).unwrap();

    assert_eq!(
        close_fact.id.confirmed_at,
        candles.last().unwrap().close_time
    );
    assert_eq!(close, candles.last().unwrap().close);
}

fn signal(signal_id: Uuid, release_id: Uuid) -> SignalDefinition {
    SignalDefinition {
        signal_id,
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at: NOW_MS,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(4100, 0), Decimal::new(4110, 0)).unwrap()),
        projected_entry: Some(Decimal::new(4105, 0)),
        stop_loss: Decimal::new(4090, 0),
        primary_target: Decimal::new(4130, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        effective_at: NOW_MS,
        detected_at: NOW_MS,
        published_at: NOW_MS,
        release_id,
        algorithm_version: "ict-v1".to_owned(),
        parameter_version: "ict-1m-v1".to_owned(),
        created_at: NOW_MS,
        expires_at: None,
        htf_context: None,
    }
}

async fn seed_signal(pool: &sqlx::PgPool, signal: &SignalDefinition) {
    sqlx::query(
        "INSERT INTO signals \
         (signal_id, symbol, plan, interval, definition, evidence, release_id, \
          algorithm_version, parameter_version, created_at) \
         VALUES ($1, $2, $3, $4, $5, '[]', $6, $7, $8, to_timestamp($9 / 1000.0))",
    )
    .bind(signal.signal_id)
    .bind(signal.symbol.as_str())
    .bind(signal.plan.as_str())
    .bind(signal.target_timeframe.as_str())
    .bind(serde_json::to_value(signal).unwrap())
    .bind(signal.release_id)
    .bind(&signal.algorithm_version)
    .bind(&signal.parameter_version)
    .bind(signal.created_at)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_directive(pool: &sqlx::PgPool, directive: &ManagementDirective) {
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
    .bind(serde_json::to_value(directive).unwrap())
    .bind(directive.effective_at)
    .bind(directive.source_closed_candle_time)
    .bind(&directive.algorithm_version)
    .bind(&directive.parameter_version)
    .execute(pool)
    .await
    .unwrap();
}

fn directive(signal_id: Uuid) -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::now_v7(),
        signal_id,
        virtual_trade_id: None,
        kind: DirectiveKind::MoveStop,
        effective_at: NOW_MS,
        price: Some(Decimal::new(4100, 0)),
        reason: "test directive".to_owned(),
        source_closed_candle_time: NOW_MS,
        algorithm_version: "ict-v1".to_owned(),
        parameter_version: "ict-1m-v1".to_owned(),
        reverse_signal: None,
    }
}

async fn prepare_database() -> (String, Uuid, OwnedMutexGuard<()>) {
    let guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .expect("truncate phase tables");
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .expect("seed market");

    let release_id = Uuid::now_v7();
    let configurations = ConfigurationRepository::new(pool.clone());
    configurations
        .create_draft(&ConfigurationRelease {
            release_id,
            symbol: symbol(),
            mode: ReleaseMode::Research,
            lifecycle: ReleaseLifecycle::Draft,
            validation_summary: Value::Null,
            profiles: vec![profile(release_id)],
        })
        .await
        .unwrap();
    configurations
        .activate(release_id, &ValidationGate::default())
        .await
        .unwrap();
    let work_id = Uuid::now_v7();
    WorkRepository::new(pool.clone())
        .enqueue(&AnalysisWorkItem {
            work_id,
            symbol: symbol(),
            timeframe: Timeframe::M1,
            canonical_close_time: 59_999,
            release_id,
            source: WorkSource::CatchUp,
            status: WorkStatus::Pending,
            lease_owner: None,
            lease_expires_at: None,
            attempt_count: 0,
            error: None,
            created_at: NOW_MS,
            completed_at: None,
        })
        .await
        .unwrap();
    pool.close().await;
    (database_url, work_id, guard)
}

#[tokio::test]
async fn pending_execution_signals_returns_new_signals_and_active_ids_are_restored_separately() {
    let _guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::now_v7();
    let active = signal(Uuid::now_v7(), release_id);
    let pending = signal(Uuid::now_v7(), release_id);
    let closed = signal(Uuid::now_v7(), release_id);
    seed_signal(&pool, &active).await;
    seed_signal(&pool, &pending).await;
    seed_signal(&pool, &closed).await;
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
    )
    .bind(active.signal_id)
    .bind(json!({
        "signalId": active.signal_id,
        "setupState": "ENTRY_AVAILABLE",
        "tradeState": "NOT_ENTERED"
    }))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
    )
    .bind(closed.signal_id)
    .bind(json!({
        "signalId": closed.signal_id,
        "setupState": "INVALIDATED",
        "tradeState": "NOT_ENTERED"
    }))
    .execute(&pool)
    .await
    .unwrap();

    let signals = runtime::pending_execution_signals(&pool, 100)
        .await
        .unwrap();

    let signal_ids = signals
        .iter()
        .map(|signal| signal.signal_id)
        .collect::<Vec<_>>();
    assert_eq!(signal_ids, vec![pending.signal_id]);
    let active_ids = runtime::active_execution_signal_ids(&pool).await.unwrap();
    assert_eq!(active_ids, vec![active.signal_id]);
    pool.close().await;
}

#[tokio::test]
async fn pending_execution_directives_scans_only_active_signal_scope() {
    let _guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::now_v7();
    for _ in 0..120 {
        let inactive = signal(Uuid::now_v7(), release_id);
        seed_signal(&pool, &inactive).await;
        seed_directive(&pool, &directive(inactive.signal_id)).await;
    }
    let active = signal(Uuid::now_v7(), release_id);
    let active_directive = directive(active.signal_id);
    seed_signal(&pool, &active).await;
    seed_directive(&pool, &active_directive).await;
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
    )
    .bind(active.signal_id)
    .bind(json!({
        "signalId": active.signal_id,
        "setupState": "ENTRY_AVAILABLE",
        "tradeState": "NOT_ENTERED"
    }))
    .execute(&pool)
    .await
    .unwrap();

    let directives = runtime::pending_execution_directives(&pool, NOW_MS, 100)
        .await
        .unwrap();
    assert_eq!(
        directives
            .iter()
            .map(|directive| directive.directive_id)
            .collect::<Vec<_>>(),
        vec![active_directive.directive_id]
    );

    let plan: Value = sqlx::query_scalar(runtime::pending_execution_directives_explain_sql())
        .bind(NOW_MS)
        .bind(100_i64)
        .fetch_one(&pool)
        .await
        .unwrap();
    let scanned_directives =
        max_actual_rows_for_relation(&plan[0]["Plan"], "trade_management_directives");
    assert!(
        scanned_directives <= 3,
        "pending directive query should not scan every stale directive; scanned {scanned_directives}: {plan}"
    );
    pool.close().await;
}

fn max_actual_rows_for_relation(plan: &Value, relation: &str) -> i64 {
    let current = if plan["Relation Name"] == relation {
        plan["Actual Rows"].as_f64().unwrap_or(0.0).ceil() as i64
    } else {
        0
    };
    let child_max = plan["Plans"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|child| max_actual_rows_for_relation(child, relation))
        .max()
        .unwrap_or(0);
    current.max(child_max)
}

#[tokio::test]
async fn runtime_bootstrap_creates_active_release_for_every_plan_and_work_hook() {
    let _guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();

    runtime::ensure_runtime_analysis_bootstrap(&pool, &symbol())
        .await
        .unwrap();
    let first_release_id: Uuid = sqlx::query_scalar(
        "SELECT release_id FROM configuration_releases \
         WHERE symbol = 'XAUUSDT' AND lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    runtime::ensure_runtime_analysis_bootstrap(&pool, &symbol())
        .await
        .unwrap();
    let second_release_id: Uuid = sqlx::query_scalar(
        "SELECT release_id FROM configuration_releases \
         WHERE symbol = 'XAUUSDT' AND lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(second_release_id, first_release_id);

    let releases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM configuration_releases WHERE lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(releases, 1);
    let active_mode: String = sqlx::query_scalar(
        "SELECT mode FROM configuration_releases WHERE symbol = 'XAUUSDT' AND lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_mode, "PAPER_ACTIVE");
    let profiles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analysis_profiles")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Eight plans on seven timeframes; the two research plans are stored disabled.
    assert_eq!(profiles, 56);
    let profile_contracts = sqlx::query(
        "SELECT plan, algorithm_version, parameter_version, position_management \
         FROM analysis_profiles ORDER BY plan, interval",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let expected_position_management =
        serde_json::to_value(analysis_domain::PositionManagementParameters::default()).unwrap();
    for row in profile_contracts {
        let plan: String = row.get("plan");
        let algorithm_version: String = row.get("algorithm_version");
        let parameter_version: String = row.get("parameter_version");
        let position_management: Value = row.get("position_management");
        let expected_algorithm = match plan.as_str() {
            "ICT" | "SMC" | "RSI" | "QUASIMODO" | "TREND_BREAKOUT" => format!("{plan}-runtime-v4"),
            "SESSION_BREAKOUT" | "VOLATILITY_SQUEEZE" => format!("{plan}-runtime-v1"),
            _ => format!("{plan}-runtime-v3"),
        };
        assert_eq!(algorithm_version, expected_algorithm);
        assert!(parameter_version.ends_with("-pp1-pc1"));
        assert_eq!(position_management, expected_position_management);
    }

    sqlx::query(
        "INSERT INTO market_sessions \
         (session_market, session_status, session_start_time, session_end_time, source_event_time) \
         VALUES ('COMMODITY', 'REGULAR', 0, 60000, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    CandleRepository::new(pool.clone())
        .persist_closed_operational(&ClosedCandleWrite {
            candle: candle(0, 0),
            source: ClosedCandleSource::Live,
        })
        .await
        .unwrap();
    let work_items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analysis_work_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(work_items, 1);
    pool.close().await;
}

#[tokio::test]
async fn runtime_bootstrap_replaces_incomplete_active_release() {
    let _guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let incomplete_release_id = Uuid::now_v7();
    let configurations = ConfigurationRepository::new(pool.clone());
    configurations
        .create_draft(&ConfigurationRelease {
            release_id: incomplete_release_id,
            symbol: symbol(),
            mode: ReleaseMode::Research,
            lifecycle: ReleaseLifecycle::Draft,
            validation_summary: Value::Null,
            profiles: vec![profile(incomplete_release_id)],
        })
        .await
        .unwrap();
    configurations
        .activate(incomplete_release_id, &ValidationGate::default())
        .await
        .unwrap();

    runtime::ensure_runtime_analysis_bootstrap(&pool, &symbol())
        .await
        .unwrap();

    let active_profiles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analysis_profiles p \
         JOIN configuration_releases r ON r.release_id = p.release_id \
         WHERE r.symbol = 'XAUUSDT' AND r.lifecycle = 'ACTIVE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_profiles, 56);
    let retired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM configuration_releases \
         WHERE release_id = $1 AND lifecycle = 'RETIRED'",
    )
    .bind(incomplete_release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retired, 1);
    pool.close().await;
}

#[tokio::test]
async fn runtime_candle_source_builds_ready_snapshot_from_database_history() {
    let _guard = test_db_guard().await;
    let database_url = test_database_url();
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let candles: Vec<_> = (0..220)
        .map(|index| candle(index * Timeframe::M1.duration_ms(), index))
        .collect();
    CandleRepository::new(pool.clone())
        .upsert_page(&candles)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO market_sessions \
         (session_market, session_status, session_start_time, session_end_time, source_event_time) \
         VALUES ('COMMODITY', 'REGULAR', $1, $2, $2)",
    )
    .bind(candles.first().unwrap().open_time)
    .bind(candles.last().unwrap().close_time + 1)
    .execute(&pool)
    .await
    .unwrap();
    let release_id = Uuid::now_v7();
    let work = AnalysisWorkItem {
        work_id: Uuid::now_v7(),
        symbol: symbol(),
        timeframe: Timeframe::M1,
        canonical_close_time: candles.last().unwrap().close_time,
        release_id,
        source: WorkSource::CatchUp,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: NOW_MS,
        completed_at: None,
    };

    let source = runtime::facts::RuntimeAnalysisCandleSource::new(
        CandleRepository::new(pool.clone()),
        market_storage::MarketRepository::new(pool.clone()),
        market_storage::SessionRepository::new(pool.clone()),
    );
    let snapshot = source.snapshot(&work, &profile(release_id)).await.unwrap();
    let SnapshotAvailability::Ready(snapshot) = snapshot else {
        panic!("runtime snapshot should be ready");
    };
    assert_eq!(snapshot.readiness, Readiness::Ready);
    assert!(!snapshot.target.facts.is_empty());
    assert!(!snapshot.primitive_versions.is_empty());
    pool.close().await;
}

fn timeframe_candle(timeframe: Timeframe, open_time: i64, step: i64) -> Candle {
    let base_cents = 410_000 + step;
    Candle::new(
        symbol(),
        timeframe,
        open_time,
        open_time + timeframe.duration_ms() - 1,
        Decimal::new(base_cents, 2),
        Decimal::new(base_cents + 350, 2),
        Decimal::new(base_cents - 250, 2),
        Decimal::new(base_cents + 150, 2),
    )
    .unwrap()
}

async fn runtime_source_pool(session_end: i64) -> sqlx::PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url())
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
    .unwrap();
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO market_sessions \
         (session_market, session_status, session_start_time, session_end_time, source_event_time) \
         VALUES ('COMMODITY', 'REGULAR', 0, $1, $1)",
    )
    .bind(session_end)
    .execute(&pool)
    .await
    .unwrap();
    pool
}

fn runtime_work(
    timeframe: Timeframe,
    canonical_close_time: i64,
    release_id: Uuid,
) -> AnalysisWorkItem {
    AnalysisWorkItem {
        work_id: Uuid::now_v7(),
        symbol: symbol(),
        timeframe,
        canonical_close_time,
        release_id,
        source: WorkSource::CatchUp,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: NOW_MS,
        completed_at: None,
    }
}

#[tokio::test]
async fn runtime_candle_source_uses_history_before_old_backlog_work() {
    let _guard = test_db_guard().await;
    let minute = Timeframe::M1.duration_ms();
    let pool = runtime_source_pool(1_300 * minute).await;
    let candles: Vec<_> = (0..1_300)
        .map(|index| timeframe_candle(Timeframe::M1, index * minute, index))
        .collect();
    CandleRepository::new(pool.clone())
        .upsert_page(&candles)
        .await
        .unwrap();
    let release_id = Uuid::now_v7();
    // 1,050 newer candles exist after this work; it must still see its own history.
    let work = runtime_work(Timeframe::M1, candles[249].close_time, release_id);

    let source = runtime::facts::RuntimeAnalysisCandleSource::new(
        CandleRepository::new(pool.clone()),
        market_storage::MarketRepository::new(pool.clone()),
        market_storage::SessionRepository::new(pool.clone()),
    );
    let SnapshotAvailability::Ready(snapshot) =
        source.snapshot(&work, &profile(release_id)).await.unwrap()
    else {
        panic!("runtime snapshot should be ready");
    };
    assert_eq!(snapshot.readiness, Readiness::Ready);
    pool.close().await;
}

#[tokio::test]
async fn runtime_candle_source_waits_for_boundary_higher_timeframe_candle() {
    let _guard = test_db_guard().await;
    let minute = Timeframe::M1.duration_ms();
    let hour = Timeframe::H1.duration_ms();
    let boundary_hours = 210;
    let pool = runtime_source_pool((boundary_hours + 1) * hour).await;
    let repository = CandleRepository::new(pool.clone());
    let minutes: Vec<_> = (boundary_hours * 60 - 250..boundary_hours * 60)
        .map(|index| timeframe_candle(Timeframe::M1, index * minute, index))
        .collect();
    repository.upsert_page(&minutes).await.unwrap();
    // The 1h candle closing at the same boundary as the 1m work is not stored yet.
    let hours: Vec<_> = (0..boundary_hours - 1)
        .map(|index| timeframe_candle(Timeframe::H1, index * hour, index))
        .collect();
    repository.upsert_page(&hours).await.unwrap();
    let release_id = Uuid::now_v7();
    let mut profile = profile(release_id);
    profile.required_timeframes = vec![Timeframe::H1];
    let work = runtime_work(Timeframe::M1, boundary_hours * hour - 1, release_id);
    let source = |grace_ms| {
        runtime::facts::RuntimeAnalysisCandleSource::new(
            CandleRepository::new(pool.clone()),
            market_storage::MarketRepository::new(pool.clone()),
            market_storage::SessionRepository::new(pool.clone()),
        )
        .with_barrier_grace_ms(grace_ms)
    };

    assert!(matches!(
        source(i64::MAX / 2)
            .snapshot(&work, &profile)
            .await
            .unwrap(),
        SnapshotAvailability::Waiting
    ));
    let SnapshotAvailability::Ready(expired) = source(0).snapshot(&work, &profile).await.unwrap()
    else {
        panic!("expired barrier should evaluate");
    };
    assert_eq!(
        expired.readiness,
        Readiness::NotReady {
            reason: analysis_domain::ReadinessReason::MissingHigherTimeframe,
        }
    );

    repository
        .upsert_page(&[timeframe_candle(
            Timeframe::H1,
            (boundary_hours - 1) * hour,
            boundary_hours - 1,
        )])
        .await
        .unwrap();
    let SnapshotAvailability::Ready(ready) = source(i64::MAX / 2)
        .snapshot(&work, &profile)
        .await
        .unwrap()
    else {
        panic!("boundary candle present should evaluate");
    };
    assert_eq!(ready.readiness, Readiness::Ready);
    pool.close().await;
}

#[tokio::test]
async fn runtime_resumes_pending_analysis_work_after_recent_sync_and_shuts_down() {
    let (database_url, work_id, _guard) = prepare_database().await;
    let fixture = spawn_fixture_server().await;
    let bind = unused_loopback_address().await;
    let config = AppConfig {
        bind,
        public_host: None,
        database_url: database_url.clone(),
        binance_rest_base_url: fixture.base_url.clone(),
        binance_ws_base_url: Url::parse("ws://127.0.0.1:9/market").unwrap(),
        static_dir: PathBuf::from("web/dist"),
        initial_history_limit: 1_000,
        backfill_page_limit: 1_500,
        analysis_worker_id: "local-1".to_owned(),
        analysis_lease_seconds: 30,
        analysis_barrier_grace_seconds: 10,
        backtest_max_concurrent_runs: 1,
        session_history: SessionHistoryConfig {
            overrides_path: PathBuf::from("config/commodity-session-overrides.json"),
            policy_version: "commodity-session-policy-v1".to_owned(),
        },
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut runtime_task = tokio::spawn(runtime::run(config, async move {
        let _ = shutdown_rx.await;
    }));

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if runtime_task.is_finished() {
                let result = (&mut runtime_task)
                    .await
                    .expect("runtime task panicked before pending analysis work completed");
                panic!("runtime exited before pending analysis work completed: {result:?}");
            }
            let status: String =
                sqlx::query_scalar("SELECT status FROM analysis_work_items WHERE work_id = $1")
                    .bind(work_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if status == "COMPLETED" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("analysis worker did not resume pending work");
    pool.close().await;

    shutdown_tx.send(()).unwrap();
    timeout(Duration::from_secs(12), runtime_task)
        .await
        .expect("runtime did not shut down")
        .expect("runtime task panicked")
        .expect("runtime returned an error");
}
