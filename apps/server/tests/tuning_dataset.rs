use gold_monitoring_server::tuning_dataset::{TuningDatasetLoader, dataset_audit_line};
use market_domain::{Candle, Market, MarketStatus, SessionStatus, Symbol, Timeframe};
use market_service::{SessionRecord, SessionSource};
use market_storage::{CandleRepository, MIGRATOR, MarketRepository, SessionRepository};
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use std::sync::{Arc, OnceLock};
use tokio::sync::OwnedMutexGuard;

static TEST_DB_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

#[tokio::test]
async fn filters_sessions_before_limit_and_produces_stable_identity() {
    let (pool, symbol, _guard) = test_pool().await;
    let sessions = SessionRepository::new(pool.clone());
    sessions
        .replace_canonical_range(
            "COMMODITY",
            0,
            240_000,
            &[
                session(SessionStatus::Regular, 0, 120_000),
                session(SessionStatus::NoTrading, 120_000, 180_000),
                session(SessionStatus::Regular, 180_000, 240_000),
            ],
        )
        .await
        .unwrap();
    CandleRepository::new(pool.clone())
        .upsert_page(&[
            candle(0, 4_100),
            candle(60_000, 4_101),
            candle(120_000, 4_102),
            candle(180_000, 4_103),
        ])
        .await
        .unwrap();
    let loader = TuningDatasetLoader::new(
        pool.clone(),
        "commodity-session-policy-v1",
        "commodity-overrides-v1",
    );

    let first = loader.load(&symbol, Timeframe::M1, None, 2).await.unwrap();
    let second = loader.load(&symbol, Timeframe::M1, None, 2).await.unwrap();

    assert_eq!(
        first
            .candles
            .iter()
            .map(|candle| candle.open_time)
            .collect::<Vec<_>>(),
        vec![60_000, 180_000]
    );
    assert_eq!(first.audit.tradable_count, 2);
    assert_eq!(first.audit.excluded_count, 1);
    assert_eq!(first.audit.unknown_count, 0);
    assert_eq!(first.audit.dataset_hash.len(), 16);
    assert_eq!(first.audit.dataset_hash, second.audit.dataset_hash);
    assert_eq!(
        dataset_audit_line(&first.audit),
        format!(
            "DATASET,XAUUSDT,1m,60000,239999,2,1,0,commodity-session-policy-v1,commodity-overrides-v1,{}",
            first.audit.dataset_hash
        )
    );
    pool.close().await;
}

#[tokio::test]
async fn unknown_session_coverage_fails_closed() {
    let (pool, symbol, _guard) = test_pool().await;
    SessionRepository::new(pool.clone())
        .replace_canonical_range(
            "COMMODITY",
            0,
            60_000,
            &[session(SessionStatus::Regular, 0, 60_000)],
        )
        .await
        .unwrap();
    CandleRepository::new(pool.clone())
        .upsert_page(&[candle(0, 4_100), candle(60_000, 4_101)])
        .await
        .unwrap();
    let loader = TuningDatasetLoader::new(
        pool.clone(),
        "commodity-session-policy-v1",
        "commodity-overrides-v1",
    );

    let error = loader
        .load(&symbol, Timeframe::M1, None, 10)
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("unknown session coverage: 1 candle")
    );
    pool.close().await;
}

async fn test_pool() -> (sqlx::PgPool, Symbol, OwnedMutexGuard<()>) {
    let guard = TEST_DB_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    sqlx::query("TRUNCATE market_sessions, candles, markets CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let symbol = Symbol::new("XAUUSDT").unwrap();
    MarketRepository::new(pool.clone())
        .upsert(
            &Market::new(
                symbol.clone(),
                true,
                MarketStatus::Trading,
                Decimal::new(1, 2),
                0,
                "COMMODITY",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    (pool, symbol, guard)
}

fn session(status: SessionStatus, start_time: i64, end_time: i64) -> SessionRecord {
    SessionRecord {
        session: market_domain::MarketSession::new("COMMODITY", status, start_time, end_time, 1)
            .unwrap(),
        source: SessionSource::DerivedWeekly,
        source_reference: Some("weekly-v1".to_owned()),
    }
}

fn candle(open_time: i64, close: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(close, 0),
        Decimal::new(close + 1, 0),
        Decimal::new(close - 1, 0),
        Decimal::new(close, 0),
    )
    .unwrap()
}
