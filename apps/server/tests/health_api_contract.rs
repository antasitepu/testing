use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use gold_monitoring_server::{
    api,
    runtime::health::{
        RuntimeCounterSnapshot, RuntimeDiagnosticsSnapshot, RuntimeHealth, RuntimePhase,
        RuntimePoolSnapshot, RuntimeTimeframeSnapshot,
    },
    state::AppState,
};
use market_domain::{Candle, ClosedCandleWrite, Market, Symbol, SyncStatus, Timeframe};
use market_service::{
    CandleStore, MarketHub, MarketStore, ServiceError, SyncCheckpoint, SyncOperation, SyncStore,
};
use std::sync::Arc;
use tower::ServiceExt;

struct UnusedStores;

#[async_trait]
impl MarketStore for UnusedStores {
    async fn upsert_market(&self, _market: &Market) -> Result<(), ServiceError> {
        panic!("health request must not call market repository")
    }

    async fn load_market(&self, _symbol: &Symbol) -> Result<Option<Market>, ServiceError> {
        panic!("health request must not call market repository")
    }

    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError> {
        panic!("health request must not call market repository")
    }
}

#[async_trait]
impl CandleStore for UnusedStores {
    async fn upsert_page(&self, _candles: &[Candle]) -> Result<(), ServiceError> {
        panic!("health request must not call candle repository")
    }

    async fn persist_page_with_checkpoint(
        &self,
        _candles: &[Candle],
        _checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call candle repository")
    }

    async fn persist_closed_operational(
        &self,
        _write: &ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call candle repository")
    }

    async fn latest(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _before: Option<i64>,
        _limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        panic!("health request must not call candle repository")
    }

    async fn bounds(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        panic!("health request must not call candle repository")
    }

    async fn open_times(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _start: i64,
        _end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        panic!("health request must not call candle repository")
    }
}

#[async_trait]
impl SyncStore for UnusedStores {
    async fn load(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_recent_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_recent_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
        _latest: i64,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_backfill_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_backfill_progress(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_backfill_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn mark_failed(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _operation: SyncOperation,
        _message: &str,
    ) -> Result<(), ServiceError> {
        panic!("health request must not call sync repository")
    }

    async fn all_for_symbol(&self, _symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError> {
        panic!("health request must not call sync repository")
    }
}

fn state(health: &RuntimeHealth) -> AppState {
    let stores = Arc::new(UnusedStores);
    AppState::new(
        stores.clone(),
        stores.clone(),
        stores,
        Arc::new(MarketHub::new(8)),
        1_000,
    )
    .with_runtime_health(health.subscribe())
}

#[tokio::test]
async fn health_routes_are_additive_and_snapshot_only() {
    let diagnostics = RuntimeDiagnosticsSnapshot {
        pool: RuntimePoolSnapshot {
            size: 8,
            idle_connections: 6,
        },
        timeframes: vec![RuntimeTimeframeSnapshot {
            timeframe: Timeframe::M1,
            latest_synced_time: Some(120_000),
            latest_closed_candle_time: Some(179_999),
            latest_evaluation_time: Some(179_999),
            analysis_backlog: 0,
        }],
        counters: RuntimeCounterSnapshot {
            recovery_count: 1,
            task_failure_count: 0,
            market_reconnect_count: 2,
            subscriber_lag_count: 3,
            ..RuntimeCounterSnapshot::default()
        },
        active_recommendation_count: 2,
        active_trade_count: 1,
        latest_market_activity: Some(179_999),
        latest_analysis_activity: Some(179_999),
        latest_execution_activity: Some(170_000),
    };
    let health = RuntimeHealth::new(diagnostics, 100);
    let app = api::router(state(&health));

    let live = app
        .clone()
        .oneshot(Request::get("/health/live").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);

    let ready = app
        .clone()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);

    health.publish(RuntimePhase::Ready, None, 200);
    let ready = app
        .clone()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    let ready_body = http_body_util::BodyExt::collect(ready.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&ready_body).unwrap(),
        serde_json::json!({"phase": "READY", "reason": null})
    );

    let diagnostics = app
        .oneshot(
            Request::get("/health/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(diagnostics.status(), StatusCode::OK);
    let diagnostics_body = http_body_util::BodyExt::collect(diagnostics.into_body())
        .await
        .unwrap()
        .to_bytes();
    let diagnostics: serde_json::Value = serde_json::from_slice(&diagnostics_body).unwrap();
    assert_eq!(diagnostics["phase"], "READY");
    assert_eq!(diagnostics["updatedAt"], 200);
    assert_eq!(diagnostics["diagnostics"]["pool"]["size"], 8);
    assert_eq!(diagnostics["diagnostics"]["pool"]["idleConnections"], 6);
    assert_eq!(
        diagnostics["diagnostics"]["timeframes"][0]["timeframe"],
        "1m"
    );
    assert_eq!(diagnostics["diagnostics"]["counters"]["recoveryCount"], 1);
    assert_eq!(
        diagnostics["diagnostics"]["counters"]["marketReconnectCount"],
        2
    );
    assert_eq!(
        diagnostics["diagnostics"]["counters"]["subscriberLagCount"],
        3
    );
    assert_eq!(diagnostics["diagnostics"]["activeRecommendationCount"], 2);
}
