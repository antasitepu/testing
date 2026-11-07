use market_domain::{
    Candle, Market, MarketSession, MarketStatus, SessionStatus, Symbol, SyncRunStatus, SyncStatus,
    Timeframe,
};
use market_service::{SessionRecord, SessionSource};
use market_storage::{
    CandleRepository, MIGRATOR, MarketRepository, SessionRepository, SyncRepository,
};
use rust_decimal::Decimal;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::time::{Duration, sleep};

async fn test_pool() -> PgPool {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    if let Ok(primary_database_url) = std::env::var("DATABASE_URL") {
        assert_ne!(
            database_url, primary_database_url,
            "TEST_DATABASE_URL must not equal DATABASE_URL"
        );
    }
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    MIGRATOR.run(&pool).await.expect("run migrations");
    sqlx::query("TRUNCATE market_sessions, sync_states, candles, markets RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .expect("truncate phase 1 tables");
    pool
}

fn market() -> Market {
    Market::new(
        Symbol::new("XAUUSDT").unwrap(),
        true,
        MarketStatus::Trading,
        Decimal::new(1, 2),
        1_756_684_800_000,
        "COMMODITY",
    )
    .unwrap()
}

fn candle(open_time: i64, close: Decimal) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        open_time,
        open_time + Timeframe::M1.duration_ms() - 1,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        close,
    )
    .unwrap()
}

#[tokio::test]
async fn repositories_are_idempotent_and_return_canonical_order() {
    let pool = test_pool().await;
    let markets = MarketRepository::new(pool.clone());
    let candles = CandleRepository::new(pool.clone());
    let sessions = SessionRepository::new(pool.clone());
    let sync = SyncRepository::new(pool.clone());
    let market = market();

    markets.upsert(&market).await.unwrap();
    assert_eq!(
        markets.load(&market.symbol).await.unwrap(),
        Some(market.clone())
    );

    candles
        .upsert_page(&[
            candle(120_000, Decimal::new(410050, 2)),
            candle(60_000, Decimal::new(410025, 2)),
        ])
        .await
        .unwrap();
    candles
        .upsert_page(&[candle(60_000, Decimal::new(410075, 2))])
        .await
        .unwrap();

    let page = candles
        .latest(&market.symbol, Timeframe::M1, None, 1_000)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].open_time, 60_000);
    assert_eq!(page[0].close, Decimal::new(410075, 2));
    assert_eq!(page[1].open_time, 120_000);
    assert_eq!(
        candles.bounds(&market.symbol, Timeframe::M1).await.unwrap(),
        Some((60_000, 120_000))
    );

    let session = MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 180_000, 10).unwrap();
    sessions
        .upsert_sessions(&[session.clone(), session.clone()])
        .await
        .unwrap();
    assert_eq!(
        sessions.covering("COMMODITY", 60_000).await.unwrap(),
        Some(session.clone())
    );
    let inserted_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT updated_at FROM market_sessions WHERE session_market = 'COMMODITY'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sleep(Duration::from_millis(5)).await;
    sessions
        .upsert_sessions(std::slice::from_ref(&session))
        .await
        .unwrap();
    let replayed_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT updated_at FROM market_sessions WHERE session_market = 'COMMODITY'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(replayed_at, inserted_at);

    let status = SyncStatus {
        symbol: market.symbol.clone(),
        timeframe: Timeframe::M1,
        oldest_synced_time: Some(60_000),
        latest_synced_time: Some(120_000),
        full_history_complete: false,
        recent_sync_status: SyncRunStatus::Complete,
        backfill_status: SyncRunStatus::Running,
        last_error: None,
    };
    sync.upsert(&status).await.unwrap();
    assert_eq!(
        sync.load(&market.symbol, Timeframe::M1).await.unwrap(),
        Some(status)
    );
    candles
        .upsert_page(&[candle(180_000, Decimal::new(410100, 2))])
        .await
        .unwrap();

    let page = candles
        .latest(&market.symbol, Timeframe::M1, Some(180_000), 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|item| item.open_time).collect::<Vec<_>>(),
        vec![60_000, 120_000]
    );
    assert_eq!(
        candles
            .open_times(&market.symbol, Timeframe::M1, 120_000, 240_000)
            .await
            .unwrap(),
        vec![120_000, 180_000]
    );
    pool.close().await;
}

#[tokio::test]
async fn market_session_rows_have_source_provenance() {
    let pool = test_pool().await;
    let sessions = SessionRepository::new(pool.clone());
    let session = MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 60_000, 10).unwrap();
    sessions.upsert_sessions(&[session]).await.unwrap();

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT source_kind, source_reference \
         FROM market_sessions WHERE session_market = 'COMMODITY'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.0, "BINANCE_API");
    assert_eq!(row.1, None);
    pool.close().await;
}

#[tokio::test]
async fn canonical_session_replace_is_idempotent_and_ordered() {
    let pool = test_pool().await;
    let repository = SessionRepository::new(pool.clone());
    let records = vec![
        session_record(
            SessionStatus::Regular,
            0,
            60_000,
            SessionSource::DerivedWeekly,
            "weekly-v1",
        ),
        session_record(
            SessionStatus::NoTrading,
            60_000,
            120_000,
            SessionSource::ManualOverride,
            "holiday-v1",
        ),
    ];

    repository
        .replace_canonical_range("COMMODITY", 0, 120_000, &records)
        .await
        .unwrap();
    repository
        .replace_canonical_range("COMMODITY", 0, 120_000, &records)
        .await
        .unwrap();

    assert_eq!(
        repository
            .canonical_between("COMMODITY", 0, 120_000)
            .await
            .unwrap(),
        records
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM market_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    pool.close().await;
}

#[tokio::test]
async fn invalid_canonical_replace_keeps_existing_rows() {
    let pool = test_pool().await;
    let repository = SessionRepository::new(pool.clone());
    let original = vec![session_record(
        SessionStatus::Regular,
        0,
        120_000,
        SessionSource::DerivedWeekly,
        "weekly-v1",
    )];
    repository
        .replace_canonical_range("COMMODITY", 0, 120_000, &original)
        .await
        .unwrap();
    let overlapping = vec![
        session_record(
            SessionStatus::Regular,
            0,
            90_000,
            SessionSource::DerivedWeekly,
            "weekly-v1",
        ),
        session_record(
            SessionStatus::NoTrading,
            60_000,
            120_000,
            SessionSource::ManualOverride,
            "holiday-v1",
        ),
    ];

    assert!(
        repository
            .replace_canonical_range("COMMODITY", 0, 120_000, &overlapping)
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .canonical_between("COMMODITY", 0, 120_000)
            .await
            .unwrap(),
        original
    );
    pool.close().await;
}

#[tokio::test]
async fn canonical_replace_preserves_outer_tails_without_boundary_overlap() {
    let pool = test_pool().await;
    let repository = SessionRepository::new(pool.clone());
    repository
        .replace_canonical_range(
            "COMMODITY",
            0,
            300_000,
            &[session_record(
                SessionStatus::Regular,
                0,
                300_000,
                SessionSource::DerivedWeekly,
                "weekly-v1",
            )],
        )
        .await
        .unwrap();

    repository
        .replace_canonical_range(
            "COMMODITY",
            60_000,
            240_000,
            &[session_record(
                SessionStatus::NoTrading,
                60_000,
                240_000,
                SessionSource::ManualOverride,
                "holiday-v1",
            )],
        )
        .await
        .unwrap();

    let rows = repository
        .canonical_between("COMMODITY", 0, 300_000)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].session.start_time, 0);
    assert_eq!(rows[0].session.end_time, 60_000);
    assert_eq!(rows[1].session.start_time, 60_000);
    assert_eq!(rows[1].session.end_time, 240_000);
    assert_eq!(rows[2].session.start_time, 240_000);
    assert_eq!(rows[2].session.end_time, 300_000);
    pool.close().await;
}

#[tokio::test]
async fn binance_session_upsert_preserves_canonical_non_overlapping_coverage() {
    let pool = test_pool().await;
    let repository = SessionRepository::new(pool.clone());
    repository
        .replace_canonical_range(
            "COMMODITY",
            0,
            300_000,
            &[
                session_record(
                    SessionStatus::Regular,
                    0,
                    120_000,
                    SessionSource::DerivedWeekly,
                    "weekly-v1",
                ),
                session_record(
                    SessionStatus::NoTrading,
                    120_000,
                    240_000,
                    SessionSource::DerivedWeekly,
                    "weekly-v1",
                ),
                session_record(
                    SessionStatus::Regular,
                    240_000,
                    300_000,
                    SessionSource::DerivedWeekly,
                    "weekly-v1",
                ),
            ],
        )
        .await
        .unwrap();

    repository
        .upsert_sessions(&[
            MarketSession::new("COMMODITY", SessionStatus::NoTrading, 120_000, 180_000, 20)
                .unwrap(),
            MarketSession::new("COMMODITY", SessionStatus::NoTrading, 150_000, 240_000, 20)
                .unwrap(),
        ])
        .await
        .unwrap();

    let rows = repository
        .canonical_between("COMMODITY", 0, 300_000)
        .await
        .unwrap();
    assert_eq!(rows.first().unwrap().session.start_time, 0);
    assert_eq!(rows.last().unwrap().session.end_time, 300_000);
    assert!(
        rows.windows(2)
            .all(|pair| pair[0].session.end_time == pair[1].session.start_time)
    );
    pool.close().await;
}

fn session_record(
    status: SessionStatus,
    start_time: i64,
    end_time: i64,
    source: SessionSource,
    source_reference: &str,
) -> SessionRecord {
    SessionRecord {
        session: MarketSession::new("COMMODITY", status, start_time, end_time, 10).unwrap(),
        source,
        source_reference: Some(source_reference.to_owned()),
    }
}
