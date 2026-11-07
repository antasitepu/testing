use crate::{
    api,
    config::AppConfig,
    execution_wake::{
        ExecutionBarrier, ExecutionWakeSource, NotifyingAnalysisWorkStore, NotifyingTrackerStore,
    },
    runtime::health::{
        RuntimeCounterSnapshot, RuntimeDiagnosticsSnapshot, RuntimeHealth, RuntimePhase,
        RuntimePoolSnapshot, RuntimeTimeframeSnapshot,
    },
    state::AppState,
};
#[cfg(test)]
use analysis_core::{
    ConfirmedSwing, DisplacementParameters, SwingParameters, calculate_adx, calculate_atr,
    calculate_bollinger, calculate_chandelier, calculate_ema, calculate_previous_donchian,
    calculate_rsi, detect_displacements_with_atr, detect_fair_value_gaps, detect_liquidity_pools,
    detect_liquidity_sweeps, detect_regular_divergence, detect_structure,
};
#[cfg(test)]
use analysis_domain::{ConfirmedFact, FactKind, FactLifecycle, Readiness};
#[cfg(test)]
use analysis_domain::{
    HtfFilterMode, IctFvgSelection, IctParameters, IctSweepBiasMode, MeanReversionParameters,
    PlanParameters, SmcParameters, TrendBreakoutParameters,
};
use analysis_domain::{PlanKind, ReleaseMode};
#[cfg(test)]
use analysis_service::GapStatusSource;
use analysis_service::{AnalysisServiceError, AnalysisWorker, ConfigurationStore};
use analysis_storage::{
    BacktestRepository, ConfigurationRepository, SignalRepository, WorkRepository,
};
#[cfg(test)]
use async_trait::async_trait;
use axum::Router;
use binance_adapter::{BinanceRestClient, BinanceWebSocket};
use execution_storage::{
    MonitoringRepository, ScalpingAuditRepository, TrackerRepository, VirtualTradeRepository,
};
use market_domain::{Symbol, Timeframe};
#[cfg(test)]
use market_service::ServiceError;
use market_service::{
    GapReconciler, HistoricalBackfill, MarketHub, MarketStore, RecentSync, StartupCoordinator,
    StreamSupervisor,
};
use market_storage::{
    CandleRepository, MIGRATOR, MarketRepository, SessionRepository, SyncRepository,
};
#[cfg(test)]
use rust_decimal::Decimal;
#[cfg(test)]
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
#[cfg(test)]
use std::collections::HashSet;
use std::{future::Future, path::Path, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::watch};
use tower_http::services::{ServeDir, ServeFile};

pub mod execution;
pub mod facts;
pub mod health;
mod market;
pub mod profiles;
pub mod recovery;
mod retention;
mod sessions;
mod tasks;

pub use execution::{
    active_execution_signal_ids, pending_directives as pending_execution_directives,
    pending_execution_directives_explain_sql, pending_signals as pending_execution_signals,
};
#[cfg(test)]
use execution::{
    process_active_next_open_canonical_opens, process_pending_execution_inputs,
    repair_terminal_recommendation_states_at_latest_close,
};
use facts::RuntimeGapStatusSource;
#[cfg(test)]
use facts::{
    BollingerRuntimeFact, DonchianRuntimeFact, fact_id, primitive_version, push_fact,
    push_fact_id_payload, push_payload_fact, push_value_fact, rsi_pivots,
};
pub use facts::{RuntimeAnalysisCandleSource, build_facts};
use profiles::{RuntimeProfileCatalog, runtime_bootstrap_gate, runtime_plan_registry};
#[cfg(test)]
use profiles::{default_parameters, default_runtime_profile, next_higher_timeframe};
#[cfg(test)]
use recovery::{recover_execution_tracking, replay_unprocessed_aggregate_trades};

#[allow(clippy::collapsible_if)]
pub async fn run<F>(
    config: AppConfig,
    shutdown: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime_health =
        RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), current_unix_millis());
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&config.database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    runtime_health.publish(
        RuntimePhase::CatchingUp,
        Some("waiting for persisted market and analysis watermarks".to_owned()),
        current_unix_millis(),
    );

    let markets = Arc::new(MarketRepository::new(pool.clone()));
    let candles = Arc::new(CandleRepository::new(pool.clone()));
    let sessions = Arc::new(SessionRepository::new(pool.clone()));
    let sync_states = Arc::new(SyncRepository::new(pool.clone()));
    let analysis_configurations = Arc::new(ConfigurationRepository::new(pool.clone()));
    let work_repository = Arc::new(
        WorkRepository::new(pool.clone())
            .with_lease_duration_ms(i64::from(config.analysis_lease_seconds) * 1_000),
    );
    let execution_wake = ExecutionWakeSource::new();
    let monitoring_wake = ExecutionWakeSource::new();
    let (execution_barrier, execution_barrier_requests) = ExecutionBarrier::channel();
    let analysis_work: Arc<dyn analysis_service::AnalysisWorkStore> = Arc::new(
        NotifyingAnalysisWorkStore::new(work_repository.clone(), execution_wake.clone())
            .with_monitoring_wake(monitoring_wake.clone())
            .with_execution_barrier(execution_barrier),
    );
    let analysis_signals = Arc::new(SignalRepository::new(pool.clone()));
    let backtests = Arc::new(BacktestRepository::new(pool.clone()));
    let tracker_repository = Arc::new(TrackerRepository::new(pool.clone()));
    let execution_tracker = Arc::new(NotifyingTrackerStore::new(
        tracker_repository,
        monitoring_wake.clone(),
    ));
    let monitoring = Arc::new(MonitoringRepository::new(pool.clone()));
    let virtual_trades = Arc::new(VirtualTradeRepository::new(pool.clone()));
    let scalping_audits = Arc::new(ScalpingAuditRepository::new(pool.clone()));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let rest = Arc::new(BinanceRestClient::new(
        http,
        config.binance_rest_base_url.clone(),
    ));
    let websocket = Arc::new(BinanceWebSocket::new(config.binance_ws_base_url.clone()));
    let hub = Arc::new(MarketHub::new(2_048));

    let recent_sync = Arc::new(RecentSync::new(
        rest.clone(),
        candles.clone(),
        sync_states.clone(),
        config.initial_history_limit,
        config.backfill_page_limit,
    ));
    let reconciler = Arc::new(GapReconciler::new(
        rest.clone(),
        candles.clone(),
        sessions.clone(),
        hub.clone(),
    ));
    let backfill = HistoricalBackfill::new(
        rest.clone(),
        candles.clone(),
        sync_states.clone(),
        config.backfill_page_limit,
    );
    let startup = StartupCoordinator::new(
        rest.clone(),
        markets.clone(),
        sessions.clone(),
        recent_sync,
        hub.clone(),
    );
    let supervisor = StreamSupervisor::new(
        websocket,
        candles.clone(),
        sessions.clone(),
        hub.clone(),
        reconciler,
    );

    let symbol = Symbol::new("XAUUSDT")?;
    let bootstrap_market = rest.market(&symbol).await?;
    markets.upsert_market(&bootstrap_market).await?;
    sessions::prepare_historical_sessions(
        &config,
        rest.clone(),
        sessions.clone(),
        candles.clone(),
        &bootstrap_market,
    )
    .await?;
    ensure_runtime_analysis_bootstrap(&pool, &symbol).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let analysis_worker = AnalysisWorker {
        work: analysis_work.clone(),
        candles: Arc::new(
            RuntimeAnalysisCandleSource::new(
                (*candles).clone(),
                (*markets).clone(),
                (*sessions).clone(),
            )
            .with_barrier_grace_ms(i64::from(config.analysis_barrier_grace_seconds) * 1_000),
        ),
        gaps: Arc::new(RuntimeGapStatusSource::new(
            markets.clone(),
            candles.clone(),
            sessions.clone(),
        )),
        configurations: analysis_configurations.clone(),
        feedback: analysis_signals.clone(),
        plans: runtime_plan_registry(),
    };
    let analysis_owner = config.analysis_worker_id.clone();
    let analysis_shutdown = shutdown_rx.clone();
    let analysis_health = runtime_health.clone();
    let mut analysis_task = tokio::spawn(async move {
        let result = tasks::publish_panic(
            &analysis_health,
            "analysis worker",
            analysis_worker.run(analysis_owner, analysis_shutdown),
        )
        .await;
        if let Err(error) = &result {
            analysis_health.publish_task_failure(
                format!("analysis worker failed: {error}"),
                current_unix_millis(),
            );
        }
        result
    });
    let execution_symbol = symbol.clone();
    let market_symbol = symbol.clone();
    let market_shutdown = shutdown_rx.clone();
    let market_health = runtime_health.clone();
    let mut market_runtime_task = market::spawn_market_runtime(
        startup,
        supervisor,
        backfill,
        market_symbol,
        market_shutdown,
        market_health,
    );
    let execution_health = runtime_health.clone();
    let execution_runtime = execution::ExecutionRuntime {
        symbol: execution_symbol,
        pool: pool.clone(),
        rest: rest.clone(),
        candles: candles.clone(),
        tracker: execution_tracker,
        market_events: hub.subscribe(),
        input_wake: execution_wake.subscribe(),
        barrier_requests: execution_barrier_requests,
        monitoring_wake: monitoring_wake.clone(),
        shutdown: shutdown_rx.clone(),
        health: execution_health.clone(),
    };
    let mut execution_task = tokio::spawn(async move {
        let result = tasks::publish_panic(
            &execution_health,
            "execution supervisor",
            execution::run_execution_loop(execution_runtime),
        )
        .await;
        if let Err(error) = &result {
            execution_health.publish_task_failure(
                format!("execution supervisor failed: {error}"),
                current_unix_millis(),
            );
        }
        result
    });

    let readiness_health = runtime_health.clone();
    let readiness_pool = pool.clone();
    let readiness_symbol = symbol.clone();
    let readiness_shutdown = shutdown_rx.clone();
    let mut readiness_task = tokio::spawn(async move {
        monitor_readiness(
            readiness_health,
            readiness_pool,
            readiness_symbol,
            readiness_shutdown,
        )
        .await;
    });

    let mut retention_task = tokio::spawn(retention::run_outbox_retention(
        monitoring.clone(),
        shutdown_rx.clone(),
    ));

    let state = AppState::new(
        markets,
        candles,
        sync_states,
        hub,
        config.initial_history_limit,
    )
    .with_analysis(analysis_signals)
    .with_backtests(backtests, analysis_configurations)
    .with_rejected_candidates(work_repository)
    .with_monitoring(monitoring)
    .with_monitoring_wake(monitoring_wake)
    .with_virtual_trades(virtual_trades)
    .with_scalping_audits(scalping_audits)
    .with_runtime_health(runtime_health.subscribe());
    let app = build_router(state, &config.static_dir, config.public_host.as_deref());
    let listener = TcpListener::bind(config.bind).await?;
    tracing::info!(address = %listener.local_addr()?, "gold monitoring server listening");

    let graceful_tx = shutdown_tx.clone();
    let graceful_health = runtime_health.clone();
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            graceful_health.publish(RuntimePhase::Stopping, None, current_unix_millis());
            let _ = graceful_tx.send(true);
        })
        .await;
    runtime_health.publish(RuntimePhase::Stopping, None, current_unix_millis());
    let _ = shutdown_tx.send(true);

    tasks::stop_service_task("market runtime", &mut market_runtime_task).await;
    tasks::stop_analysis_task("analysis worker", &mut analysis_task).await;
    tasks::stop_execution_task("execution supervisor", &mut execution_task).await;
    if !readiness_task.is_finished() {
        if tokio::time::timeout(Duration::from_secs(1), &mut readiness_task)
            .await
            .is_err()
        {
            readiness_task.abort();
            let _ = readiness_task.await;
        }
    }
    if tokio::time::timeout(Duration::from_secs(1), &mut retention_task)
        .await
        .is_err()
    {
        retention_task.abort();
        let _ = retention_task.await;
    }
    pool.close().await;
    server_result?;
    Ok(())
}

fn current_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

async fn monitor_readiness(
    health: RuntimeHealth,
    pool: PgPool,
    symbol: Symbol,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = interval.tick() => {
                let now = current_unix_millis();
                match load_runtime_diagnostics(&health, &pool, &symbol, now).await {
                    Ok((diagnostics, ready, reason)) => {
                        let phase = if ready {
                            RuntimePhase::Ready
                        } else {
                            RuntimePhase::CatchingUp
                        };
                        let reason = if ready { None } else { reason };
                        health.publish_diagnostics(phase, reason, diagnostics, now);
                    }
                    Err(error) => {
                        health.publish_readiness_failure(
                            format!("readiness persistence check failed: {error}"),
                            now,
                        );
                    }
                }
            }
        }
    }
}

const MARKET_DATA_STALE_AFTER_MS: i64 = 180_000;

const RUNTIME_DIAGNOSTICS_SQL: &str = "WITH active_release AS ( \
         SELECT ars.active_release_id AS release_id \
         FROM analysis_runtime_state ars \
         WHERE ars.symbol = $1 AND ars.active_release_id IS NOT NULL \
     ), active_profiles AS ( \
         SELECT p.symbol, p.plan, p.interval, p.release_id \
         FROM analysis_profiles p \
         JOIN active_release ar ON ar.release_id = p.release_id \
         WHERE p.symbol = $1 AND p.enabled = TRUE \
     ), profile_health AS ( \
         SELECT ap.interval, COUNT(*) AS configured_profile_count, \
                COUNT(prs.last_evaluated_close_time) AS evaluated_profile_count, \
                MIN(prs.last_evaluated_close_time) AS latest_evaluation_time \
         FROM active_profiles ap \
         LEFT JOIN plan_runtime_states prs \
           ON prs.symbol = ap.symbol AND prs.plan = ap.plan \
          AND prs.interval = ap.interval AND prs.release_id = ap.release_id \
         GROUP BY ap.interval \
     ), backlog AS ( \
         SELECT awi.interval, COUNT(*) AS analysis_backlog \
         FROM analysis_work_items awi \
         JOIN active_release ar ON awi.release_id = ar.release_id \
         WHERE awi.symbol = $1 AND awi.status IN ('PENDING', 'LEASED') \
         GROUP BY awi.interval \
     ) \
     SELECT ph.interval, ph.configured_profile_count, ph.evaluated_profile_count, \
            ph.latest_evaluation_time, ss.latest_synced_time, ss.recent_sync_status, \
            ch.latest_open_time, ch.latest_closed_candle_time, \
            aw.latest_analysis_work_time, \
            COALESCE(b.analysis_backlog, 0) AS analysis_backlog \
     FROM profile_health ph \
     LEFT JOIN sync_states ss ON ss.symbol = $1 AND ss.interval = ph.interval \
     LEFT JOIN LATERAL ( \
         SELECT c.open_time AS latest_open_time, \
                c.close_time AS latest_closed_candle_time \
         FROM candles c \
         WHERE c.symbol = $1 AND c.interval = ph.interval \
         ORDER BY c.open_time DESC \
         LIMIT 1 \
     ) ch ON TRUE \
     LEFT JOIN LATERAL ( \
         SELECT awi.close_time AS latest_analysis_work_time \
         FROM analysis_work_items awi \
         JOIN active_release ar ON awi.release_id = ar.release_id \
         WHERE awi.symbol = $1 AND awi.interval = ph.interval \
         ORDER BY awi.close_time DESC \
         LIMIT 1 \
     ) aw ON TRUE \
     LEFT JOIN backlog b ON b.interval = ph.interval \
     ORDER BY CASE ph.interval \
         WHEN '1m' THEN 1 WHEN '5m' THEN 2 WHEN '15m' THEN 3 \
         WHEN '30m' THEN 4 WHEN '1h' THEN 5 WHEN '4h' THEN 6 WHEN '1d' THEN 7 \
     END";

async fn load_runtime_diagnostics(
    health: &RuntimeHealth,
    pool: &PgPool,
    symbol: &Symbol,
    now: i64,
) -> Result<(RuntimeDiagnosticsSnapshot, bool, Option<String>), sqlx::Error> {
    let rows = sqlx::query(RUNTIME_DIAGNOSTICS_SQL)
        .bind(symbol.as_str())
        .fetch_all(pool)
        .await?;

    let active_recommendation_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ( \
             SELECT DISTINCT ON (s.symbol, s.plan, s.interval) s.signal_id \
             FROM signals s \
             JOIN tracker_states ts ON ts.signal_id = s.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE s.symbol = $1 \
               AND ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
             ORDER BY s.symbol, s.plan, s.interval, s.created_at DESC, s.signal_id DESC \
         ) active_recommendations",
    )
    .bind(symbol.as_str())
    .fetch_one(pool)
    .await?;
    let active_trade_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM virtual_trades WHERE symbol = $1 AND trade_state = 'OPEN'",
    )
    .bind(symbol.as_str())
    .fetch_one(pool)
    .await?;
    let latest_execution_activity: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(last_trade_time) FROM market_event_cursors WHERE symbol = $1",
    )
    .bind(symbol.as_str())
    .fetch_one(pool)
    .await?;
    let latest_no_trading_end: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(ms.session_end_time) FROM market_sessions ms \
         JOIN markets m ON m.session_market = ms.session_market \
         WHERE m.symbol = $1 AND ms.session_status = 'NO_TRADING' \
           AND ms.session_start_time <= $2",
    )
    .bind(symbol.as_str())
    .bind(now)
    .fetch_one(pool)
    .await?;

    let mut ready = rows.len() == Timeframe::ALL.len();
    let mut reason = if ready {
        None
    } else {
        Some(format!(
            "configured timeframe health is incomplete: expected {}, found {}",
            Timeframe::ALL.len(),
            rows.len()
        ))
    };
    let mut timeframes = Vec::with_capacity(rows.len());
    let mut latest_market_activity = None;
    let mut latest_analysis_activity = None;

    for row in rows {
        let interval: String = row.get("interval");
        let timeframe = interval.parse::<Timeframe>().map_err(|error| {
            sqlx::Error::Protocol(format!("invalid configured timeframe {interval}: {error}"))
        })?;
        let configured_profile_count: i64 = row.get("configured_profile_count");
        let evaluated_profile_count: i64 = row.get("evaluated_profile_count");
        let latest_synced_time: Option<i64> = row.get("latest_synced_time");
        let recent_sync_status: Option<String> = row.get("recent_sync_status");
        let latest_open_time: Option<i64> = row.get("latest_open_time");
        let latest_closed_candle_time: Option<i64> = row.get("latest_closed_candle_time");
        let latest_evaluation_time: Option<i64> = row.get("latest_evaluation_time");
        let latest_analysis_work_time: Option<i64> = row.get("latest_analysis_work_time");
        let analysis_backlog: i64 = row.get("analysis_backlog");

        let sync_ready = recent_sync_status.as_deref() == Some("complete")
            && latest_synced_time.is_some()
            && latest_open_time.is_some()
            && latest_synced_time >= latest_open_time;
        let evaluation_ready = evaluation_is_current(
            configured_profile_count,
            evaluated_profile_count,
            latest_evaluation_time,
            latest_analysis_work_time,
            timeframe.duration_ms(),
        );
        if !sync_ready && reason.is_none() {
            reason = Some(format!(
                "{} recent sync is incomplete or behind its latest persisted candle",
                timeframe
            ));
        } else if !evaluation_ready && reason.is_none() {
            reason = Some(format!(
                "{} analysis evaluation is more than one timeframe behind",
                timeframe
            ));
        }
        ready &= sync_ready && evaluation_ready;
        latest_market_activity = latest_market_activity.max(latest_closed_candle_time);
        latest_analysis_activity = latest_analysis_activity.max(latest_evaluation_time);
        timeframes.push(RuntimeTimeframeSnapshot {
            timeframe,
            latest_synced_time,
            latest_closed_candle_time,
            latest_evaluation_time,
            analysis_backlog: u64::try_from(analysis_backlog).unwrap_or(0),
        });
    }

    let latest_m1_close_time = timeframes
        .iter()
        .find(|snapshot| snapshot.timeframe == Timeframe::M1)
        .and_then(|snapshot| snapshot.latest_closed_candle_time);
    if !market_data_is_current(latest_m1_close_time, latest_no_trading_end, now) {
        ready = false;
        reason.get_or_insert_with(|| "1m candles have stopped arriving".to_owned());
    }

    let current = health.snapshot();
    let observability = runtime_observability::snapshot();
    latest_market_activity = latest_market_activity.max(observability.latest_market_activity);
    latest_analysis_activity = latest_analysis_activity.max(observability.latest_analysis_activity);
    let latest_execution_activity =
        latest_execution_activity.max(observability.latest_execution_activity);
    Ok((
        RuntimeDiagnosticsSnapshot {
            pool: RuntimePoolSnapshot {
                size: pool.size(),
                idle_connections: u32::try_from(pool.num_idle()).unwrap_or(u32::MAX),
            },
            timeframes,
            counters: RuntimeCounterSnapshot {
                recovery_count: current.diagnostics.counters.recovery_count,
                task_failure_count: current.diagnostics.counters.task_failure_count,
                market_reconnect_count: observability.market_reconnect_count,
                market_resync_count: observability.market_resync_count,
                execution_exact_replay_recovery_count: observability
                    .execution_exact_replay_recovery_count,
                execution_ohlc_reconciled_recovery_count: observability
                    .execution_ohlc_reconciled_recovery_count,
                subscriber_lag_count: observability.subscriber_lag_count,
                db_acquire_timeout_count: observability.db_acquire_timeout_count,
            },
            active_recommendation_count: u64::try_from(active_recommendation_count).unwrap_or(0),
            active_trade_count: u64::try_from(active_trade_count).unwrap_or(0),
            latest_market_activity,
            latest_analysis_activity,
            latest_execution_activity,
        },
        ready,
        reason,
    ))
}

/// Closed sessions produce no live candles, so lateness is measured from the reopening.
fn market_data_is_current(
    latest_m1_close_time: Option<i64>,
    latest_no_trading_end: Option<i64>,
    now: i64,
) -> bool {
    latest_m1_close_time
        .max(latest_no_trading_end)
        .is_some_and(|reference| now.saturating_sub(reference) <= MARKET_DATA_STALE_AFTER_MS)
}

fn evaluation_is_current(
    configured_profile_count: i64,
    evaluated_profile_count: i64,
    latest_evaluation_time: Option<i64>,
    latest_analysis_work_time: Option<i64>,
    timeframe_duration_ms: i64,
) -> bool {
    configured_profile_count > 0
        && evaluated_profile_count == configured_profile_count
        && latest_evaluation_time.is_some()
        && latest_analysis_work_time.is_some()
        && latest_evaluation_time
            >= latest_analysis_work_time.map(|closed| closed.saturating_sub(timeframe_duration_ms))
}

pub async fn ensure_runtime_analysis_bootstrap(
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<(), AnalysisServiceError> {
    let catalog = RuntimeProfileCatalog::current()?;
    let active_release = sqlx::query(
        "SELECT r.release_id, r.mode, COUNT(p.*) AS profile_count \
         FROM configuration_releases r \
         LEFT JOIN analysis_profiles p ON p.release_id = r.release_id \
         WHERE r.symbol = $1 AND r.lifecycle = 'ACTIVE' \
         GROUP BY r.release_id, r.mode",
    )
    .bind(symbol.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| AnalysisServiceError::Storage {
        operation: "load active runtime analysis release",
        message: error.to_string(),
    })?;
    let release_id = if let Some(row) = active_release {
        let release_id: uuid::Uuid = row.get("release_id");
        let mode: String = row.get("mode");
        let profile_count: i64 = row.get("profile_count");
        if mode == ReleaseMode::PaperActive.as_str()
            && profile_count == (PlanKind::ALL.len() * Timeframe::ALL.len()) as i64
        {
            let repository = ConfigurationRepository::new(pool.clone());
            let profiles = repository.profiles_for_release(release_id, symbol).await?;
            if catalog.matches_release(release_id, symbol, &profiles) {
                release_id
            } else {
                create_runtime_release(pool, symbol.clone(), &catalog).await?
            }
        } else {
            create_runtime_release(pool, symbol.clone(), &catalog).await?
        }
    } else {
        create_runtime_release(pool, symbol.clone(), &catalog).await?
    };
    sqlx::query(
        "INSERT INTO analysis_runtime_state \
         (symbol, live_analysis_start_at, active_release_id) VALUES ($1, 0, $2) \
         ON CONFLICT (symbol) DO UPDATE SET active_release_id = $2, updated_at = NOW()",
    )
    .bind(symbol.as_str())
    .bind(release_id)
    .execute(pool)
    .await
    .map_err(|error| AnalysisServiceError::Storage {
        operation: "upsert runtime analysis state",
        message: error.to_string(),
    })?;
    Ok(())
}

async fn create_runtime_release(
    pool: &PgPool,
    symbol: Symbol,
    catalog: &RuntimeProfileCatalog,
) -> Result<uuid::Uuid, AnalysisServiceError> {
    let repository = ConfigurationRepository::new(pool.clone());
    let release = catalog.materialize_release(symbol)?;
    let release_id = release.release_id;
    repository.create_draft(&release).await?;
    repository
        .activate(release_id, &runtime_bootstrap_gate())
        .await?;
    Ok(release_id)
}

pub fn build_router(state: AppState, static_dir: &Path, public_host: Option<&str>) -> Router {
    let static_files =
        ServeDir::new(static_dir).not_found_service(ServeFile::new(static_dir.join("index.html")));
    Router::new()
        .merge(api::router(state))
        .fallback_service(static_files)
        .layer(axum::middleware::from_fn_with_state(
            public_host.map(Arc::<str>::from),
            crate::loopback_guard::require_loopback_client,
        ))
}

#[cfg(test)]
static TEST_DB_LOCK: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();

#[cfg(test)]
mod profile_contract_tests;

#[cfg(test)]
mod runtime_health_query_tests {
    use super::{
        MARKET_DATA_STALE_AFTER_MS, RUNTIME_DIAGNOSTICS_SQL, evaluation_is_current,
        market_data_is_current,
    };

    const NOW: i64 = 1_000_000_000;
    const ONE_DAY_MS: i64 = 86_400_000;
    const TOO_LATE: i64 = NOW - MARKET_DATA_STALE_AFTER_MS - 1;

    #[test]
    fn market_data_is_current_while_candles_keep_closing() {
        assert!(market_data_is_current(Some(NOW - 60_000), None, NOW));
    }

    #[test]
    fn market_data_is_late_when_candles_stop_closing() {
        assert!(!market_data_is_current(Some(TOO_LATE), None, NOW));
        assert!(!market_data_is_current(None, None, NOW));
    }

    #[test]
    fn market_data_is_not_late_while_the_market_is_closed() {
        let closes_tomorrow = Some(NOW + ONE_DAY_MS);

        assert!(market_data_is_current(
            Some(NOW - ONE_DAY_MS),
            closes_tomorrow,
            NOW
        ));
    }

    #[test]
    fn market_data_lateness_is_measured_from_reopening() {
        let old_candle = Some(NOW - ONE_DAY_MS);

        assert!(market_data_is_current(old_candle, Some(NOW - 60_000), NOW));
        assert!(!market_data_is_current(old_candle, Some(TOO_LATE), NOW));
    }

    #[test]
    fn evaluation_readiness_tracks_latest_analysis_work_not_market_closed_candles() {
        assert!(evaluation_is_current(
            6,
            6,
            Some(1_000),
            Some(1_000),
            60_000
        ));
    }

    #[test]
    fn runtime_diagnostics_uses_indexed_latest_candle_and_active_release_backlog() {
        assert!(
            RUNTIME_DIAGNOSTICS_SQL.contains("LEFT JOIN LATERAL"),
            "latest candle lookup must be scoped to each configured timeframe"
        );
        assert!(
            RUNTIME_DIAGNOSTICS_SQL.contains("ORDER BY c.open_time DESC")
                && RUNTIME_DIAGNOSTICS_SQL.contains("LIMIT 1"),
            "latest candle lookup must use the descending candle history index"
        );
        assert!(
            !RUNTIME_DIAGNOSTICS_SQL.contains("MAX(open_time)")
                && !RUNTIME_DIAGNOSTICS_SQL.contains("MAX(close_time)"),
            "readiness must not aggregate the full candle history"
        );
        assert!(
            RUNTIME_DIAGNOSTICS_SQL.contains("awi.release_id = ar.release_id"),
            "analysis backlog must be restricted to the active release"
        );
        assert!(
            RUNTIME_DIAGNOSTICS_SQL.contains("latest_analysis_work_time"),
            "readiness must use the latest candle selected for analysis"
        );
    }
}

#[cfg(test)]
#[path = "baseline_tests.rs"]
mod baseline_backtests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
#[path = "tuning_tests.rs"]
mod tuning_backtests;
