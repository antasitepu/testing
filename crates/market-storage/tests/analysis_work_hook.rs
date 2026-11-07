use market_domain::{
    Candle, ClosedCandleSource, ClosedCandleWrite, MarketSession, SessionStatus, Symbol, Timeframe,
};
use market_storage::{
    CandleRepository, MIGRATOR, SessionRepository, StorageCheckpoint, StorageSyncOperation,
};
use rust_decimal::Decimal;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

async fn test_pool() -> PgPool {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(2)
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
    pool
}

fn candle(open_time: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        open_time,
        open_time + Timeframe::M1.duration_ms() - 1,
        Decimal::new(410_000, 2),
        Decimal::new(410_200, 2),
        Decimal::new(409_800, 2),
        Decimal::new(410_100, 2),
    )
    .unwrap()
}

async fn configure_runtime(pool: &PgPool, release_id: Uuid, active: bool) {
    sqlx::query(
        "INSERT INTO configuration_releases \
         (release_id, symbol, mode, lifecycle) VALUES ($1, 'XAUUSDT', 'RESEARCH', 'ACTIVE')",
    )
    .bind(release_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO analysis_runtime_state \
         (symbol, live_analysis_start_at, active_release_id) VALUES ('XAUUSDT', 100000, $1)",
    )
    .bind(active.then_some(release_id))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn operational_candles_create_exactly_one_eligible_work_item() {
    let pool = test_pool().await;
    let repository = CandleRepository::new(pool.clone());
    let release_id = Uuid::now_v7();
    configure_runtime(&pool, release_id, true).await;
    SessionRepository::new(pool.clone())
        .upsert_sessions(&[
            MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 360_000, 1).unwrap(),
        ])
        .await
        .unwrap();

    repository
        .persist_closed_operational(&ClosedCandleWrite {
            candle: candle(0),
            source: ClosedCandleSource::Live,
        })
        .await
        .unwrap();

    let live = ClosedCandleWrite {
        candle: candle(60_000),
        source: ClosedCandleSource::Live,
    };
    repository.persist_closed_operational(&live).await.unwrap();
    repository.persist_closed_operational(&live).await.unwrap();

    let catch_up = ClosedCandleWrite {
        candle: candle(120_000),
        source: ClosedCandleSource::CatchUp,
    };
    repository
        .persist_closed_operational(&catch_up)
        .await
        .unwrap();

    let downtime = ClosedCandleWrite {
        candle: candle(180_000),
        source: ClosedCandleSource::DowntimeReconciled,
    };
    repository
        .persist_closed_operational(&downtime)
        .await
        .unwrap();

    let historical = ClosedCandleWrite {
        candle: candle(240_000),
        source: ClosedCandleSource::HistoricalBackfill,
    };
    repository
        .persist_closed_operational(&historical)
        .await
        .unwrap();

    let rows =
        sqlx::query("SELECT close_time, source FROM analysis_work_items ORDER BY close_time")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get::<String, _>("source"), "LIVE");
    assert_eq!(rows[1].get::<String, _>("source"), "CATCH_UP");
    assert_eq!(rows[2].get::<String, _>("source"), "DOWNTIME_RECONCILED");

    let candle_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(candle_count, 5);

    sqlx::query(
        "UPDATE analysis_runtime_state SET active_release_id = NULL WHERE symbol = 'XAUUSDT'",
    )
    .execute(&pool)
    .await
    .unwrap();
    repository
        .persist_closed_operational(&ClosedCandleWrite {
            candle: candle(300_000),
            source: ClosedCandleSource::Live,
        })
        .await
        .unwrap();
    let work_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analysis_work_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(work_count, 3, "no active release creates no work");

    pool.close().await;
}

#[tokio::test]
async fn catch_up_page_persists_candles_work_and_checkpoint_together() {
    let pool = test_pool().await;
    let repository = CandleRepository::new(pool.clone());
    configure_runtime(&pool, Uuid::now_v7(), true).await;
    SessionRepository::new(pool.clone())
        .upsert_sessions(&[
            MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 360_000, 1).unwrap(),
        ])
        .await
        .unwrap();
    let writes: Vec<_> = [0, 60_000, 120_000]
        .into_iter()
        .map(|open_time| ClosedCandleWrite {
            candle: candle(open_time),
            source: ClosedCandleSource::CatchUp,
        })
        .collect();

    repository
        .persist_closed_page_with_checkpoint(
            &writes,
            &StorageCheckpoint {
                symbol: Symbol::new("XAUUSDT").unwrap(),
                timeframe: Timeframe::M1,
                oldest_synced_time: 0,
                latest_synced_time: 120_000,
                operation: StorageSyncOperation::Recent,
                complete: true,
            },
        )
        .await
        .unwrap();

    let work_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analysis_work_items WHERE source = 'CATCH_UP'")
            .fetch_one(&pool)
            .await
            .unwrap();
    // The first candle closes before live_analysis_start_at, like the single-write path.
    assert_eq!(work_count, 2);
    let candle_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(candle_count, 3);
    let latest: i64 = sqlx::query_scalar(
        "SELECT latest_synced_time FROM sync_states WHERE symbol = 'XAUUSDT' AND interval = '1m'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(latest, 120_000);
    pool.close().await;
}

#[tokio::test]
async fn no_trading_candle_is_stored_without_analysis_work() {
    let pool = test_pool().await;
    let repository = CandleRepository::new(pool.clone());
    let release_id = Uuid::now_v7();
    configure_runtime(&pool, release_id, true).await;
    SessionRepository::new(pool.clone())
        .upsert_sessions(&[MarketSession::new(
            "COMMODITY",
            SessionStatus::NoTrading,
            120_000,
            180_000,
            1,
        )
        .unwrap()])
        .await
        .unwrap();

    repository
        .persist_closed_operational(&ClosedCandleWrite {
            candle: candle(120_000),
            source: ClosedCandleSource::Live,
        })
        .await
        .unwrap();

    let candle_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candles")
        .fetch_one(&pool)
        .await
        .unwrap();
    let work_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analysis_work_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(candle_count, 1);
    assert_eq!(work_count, 0);
    pool.close().await;
}
