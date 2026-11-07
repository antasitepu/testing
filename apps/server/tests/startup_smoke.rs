use axum::{
    Json, Router,
    extract::OriginalUri,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use gold_monitoring_server::{config::AppConfig, runtime, session_history::SessionHistoryConfig};
use market_storage::MIGRATOR;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use std::{path::PathBuf, time::Duration};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::timeout};
use url::Url;

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

async fn isolated_database_url() -> String {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    assert!(
        !database_url.trim().is_empty(),
        "TEST_DATABASE_URL must not be empty"
    );
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    MIGRATOR.run(&pool).await.expect("run migrations");
    sqlx::query("TRUNCATE market_sessions, sync_states, candles, markets RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .expect("truncate isolated Phase 1 tables");
    pool.close().await;
    database_url
}

async fn unused_loopback_address() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

#[tokio::test]
async fn starts_api_from_mock_binance_and_shuts_down() {
    let database_url = isolated_database_url().await;
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

    let client = reqwest::Client::new();
    let endpoint = format!("http://{bind}/api/v1/markets");
    let markets = timeout(Duration::from_secs(10), async {
        loop {
            if runtime_task.is_finished() {
                let result = (&mut runtime_task).await.expect("runtime task panicked");
                panic!("runtime exited before HTTP became ready: {result:?}");
            }
            match client.get(&endpoint).send().await {
                Ok(response) if response.status().is_success() => {
                    break response.json::<Value>().await.unwrap();
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("server did not become ready");
    assert_eq!(markets[0]["symbol"], "XAUUSDT");
    assert_eq!(
        markets[0]["intervals"],
        serde_json::json!(["1m", "5m", "15m", "30m", "1h", "4h", "1d"])
    );
    let live = client
        .get(format!("http://{bind}/health/live"))
        .send()
        .await
        .expect("request live health");
    assert_eq!(live.status(), StatusCode::OK);
    let live = live.json::<Value>().await.expect("decode live health");
    assert!(
        matches!(
            live["phase"].as_str(),
            Some("CATCHING_UP" | "READY" | "DEGRADED")
        ),
        "unexpected live health phase: {live}"
    );

    shutdown_tx.send(()).unwrap();
    timeout(Duration::from_secs(12), runtime_task)
        .await
        .expect("runtime did not shut down")
        .expect("runtime task panicked")
        .expect("runtime returned an error");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let derived: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM market_sessions WHERE source_kind = 'DERIVED_WEEKLY'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(derived > 0);
    pool.close().await;
}
