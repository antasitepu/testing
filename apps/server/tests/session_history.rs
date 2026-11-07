use gold_monitoring_server::session_history::{
    HistoricalSessionCoordinator, SessionHistoryConfig, detect_regular_flat_runs,
    parse_session_overrides,
};
use market_domain::{Candle, MarketSession, SessionStatus, Symbol, Timeframe};
use market_service::SessionSource;
use market_storage::{MIGRATOR, SessionRepository};
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;

#[test]
fn parses_versioned_overrides_and_rejects_overlap() {
    let valid = r#"{
        "version": "commodity-overrides-v1",
        "overrides": [{
            "sessionMarket": "COMMODITY",
            "status": "NO_TRADING",
            "startTime": 1000,
            "endTime": 2000,
            "reference": "holiday-1",
            "reason": "verified holiday closure"
        }]
    }"#;
    let parsed = parse_session_overrides(valid, "COMMODITY").unwrap();
    assert_eq!(parsed.version, "commodity-overrides-v1");
    assert_eq!(parsed.records.len(), 1);
    assert_eq!(parsed.records[0].source, SessionSource::ManualOverride);
    assert_eq!(
        parsed.records[0].source_reference.as_deref(),
        Some("holiday-1")
    );

    let overlapping = valid.replace(
        "]\n    }",
        r#", {
            "sessionMarket": "COMMODITY",
            "status": "REGULAR",
            "startTime": 1500,
            "endTime": 2500,
            "reference": "holiday-2",
            "reason": "conflicting override"
        }]
    }"#,
    );
    assert!(parse_session_overrides(&overlapping, "COMMODITY").is_err());
}

#[tokio::test]
async fn coordinator_merges_official_schedule_and_override_without_unknown_coverage() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    sqlx::query("TRUNCATE market_sessions")
        .execute(&pool)
        .await
        .unwrap();
    let repository = SessionRepository::new(pool.clone());
    let start = 1_767_571_200_000;
    let official_start = start + 86_400_000;
    let end = official_start + 86_400_000;
    repository
        .upsert_sessions(&[MarketSession::new(
            "COMMODITY",
            SessionStatus::Regular,
            official_start,
            end,
            end,
        )
        .unwrap()])
        .await
        .unwrap();
    let overrides = parse_session_overrides(
        &format!(
            r#"{{
                "version": "commodity-overrides-v1",
                "overrides": [{{
                    "sessionMarket": "COMMODITY",
                    "status": "NO_TRADING",
                    "startTime": {},
                    "endTime": {},
                    "reference": "holiday-test",
                    "reason": "verified test closure"
                }}]
            }}"#,
            start + 3_600_000,
            start + 7_200_000
        ),
        "COMMODITY",
    )
    .unwrap();
    let coordinator = HistoricalSessionCoordinator::new(
        repository.clone(),
        SessionHistoryConfig {
            overrides_path: PathBuf::from("unused-in-test.json"),
            policy_version: "commodity-session-policy-v1".to_owned(),
        },
        overrides,
    );

    let report = coordinator
        .ensure_coverage("COMMODITY", start, end, end, true)
        .await
        .unwrap();

    assert_eq!(report.unknown_ms, 0);
    assert_eq!(report.conflicts, 0);
    assert_eq!(report.policy_version, "commodity-session-policy-v1");
    assert_eq!(report.override_version, "commodity-overrides-v1");
    let records = repository
        .canonical_between("COMMODITY", start, end)
        .await
        .unwrap();
    assert!(records.iter().any(|record| {
        record.source == SessionSource::ManualOverride
            && record.source_reference.as_deref() == Some("holiday-test")
    }));
    assert!(records.iter().any(|record| {
        record.source == SessionSource::BinanceApi && record.session.start_time >= official_start
    }));
    pool.close().await;
}

#[test]
fn flat_run_audit_reports_regular_runs_without_mutating_sessions() {
    let sessions = vec![
        MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 180_000, 1).unwrap(),
        MarketSession::new("COMMODITY", SessionStatus::NoTrading, 180_000, 240_000, 1).unwrap(),
    ];
    let candles = vec![
        flat_candle(0, 4_100),
        flat_candle(60_000, 4_100),
        flat_candle(120_000, 4_100),
        flat_candle(180_000, 4_100),
    ];

    let runs = detect_regular_flat_runs(&candles, &sessions, 120_000);

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].start_time, 0);
    assert_eq!(runs[0].end_time, 180_000);
    assert_eq!(runs[0].duration_ms, 180_000);
    assert_eq!(runs[0].price, Decimal::new(4_100, 0));
}

fn flat_candle(open_time: i64, price: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(price, 0),
        Decimal::new(price, 0),
        Decimal::new(price, 0),
        Decimal::new(price, 0),
    )
    .unwrap()
}
