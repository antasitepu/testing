use analysis_domain::{
    CurrentPlanProjection, Direction, ExecutionInstruction, PlanKind, PlanProfileKey, Readiness,
    SetupState, SignalDefinition, TrackingHealth, TradeState, VersionIdentity,
};
use analysis_service::{
    AnalysisServiceError, MonitoringEvent, MonitoringEventRecord, SignalHistoryQuery, SignalPage,
    SignalStore,
};
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use futures_util::StreamExt;
use gold_monitoring_server::{api, state::AppState};
use http_body_util::BodyExt;
use market_domain::{
    Candle, LiveStatus, Market, MarketStatus, Symbol, SyncRunStatus, SyncStatus, Timeframe,
};
use market_service::{
    CandleStore, MarketHub, MarketStore, ServiceError, SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
};
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;

struct FakeMarkets {
    market: Market,
}

#[async_trait]
impl MarketStore for FakeMarkets {
    async fn upsert_market(&self, _market: &Market) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn load_market(&self, symbol: &Symbol) -> Result<Option<Market>, ServiceError> {
        Ok((&self.market.symbol == symbol).then(|| self.market.clone()))
    }

    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError> {
        Ok(vec![self.market.clone()])
    }
}

struct FakeCandles {
    fail: AtomicBool,
    last_limit: AtomicI64,
    rows: Vec<Candle>,
}

#[async_trait]
impl CandleStore for FakeCandles {
    async fn upsert_page(&self, _candles: &[Candle]) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn persist_page_with_checkpoint(
        &self,
        _candles: &[Candle],
        _checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn persist_closed_operational(
        &self,
        _write: &market_domain::ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn latest(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ServiceError::Storage {
                operation: "fake latest",
                message: "database unavailable".to_owned(),
            });
        }
        self.last_limit.store(limit, Ordering::SeqCst);
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .filter(|candle| before.is_none_or(|value| candle.open_time < value))
            .cloned()
            .collect();
        let start = rows.len().saturating_sub(limit as usize);
        Ok(rows.split_off(start))
    }

    async fn bounds(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        Ok(None)
    }

    async fn open_times(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _start: i64,
        _end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        Ok(Vec::new())
    }
}

struct FakeSync {
    status: SyncStatus,
}

struct FakeSignals {
    fail: AtomicBool,
    stream_queries: AtomicUsize,
    projection: CurrentPlanProjection,
    signal: SignalDefinition,
}

#[async_trait]
impl SignalStore for FakeSignals {
    async fn active_projection(
        &self,
        _key: &PlanProfileKey,
    ) -> Result<CurrentPlanProjection, AnalysisServiceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AnalysisServiceError::Storage {
                operation: "fake projection",
                message: "database unavailable".to_owned(),
            });
        }
        Ok(self.projection.clone())
    }

    async fn history(&self, query: SignalHistoryQuery) -> Result<SignalPage, AnalysisServiceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AnalysisServiceError::Storage {
                operation: "fake history",
                message: "database unavailable".to_owned(),
            });
        }
        Ok(SignalPage {
            items: vec![self.signal.clone()],
            next_before_created_at: query.before_created_at.or(Some(240_000)),
        })
    }

    async fn monitoring_events_after(
        &self,
        cursor: i64,
        _limit: u32,
    ) -> Result<Vec<MonitoringEventRecord>, AnalysisServiceError> {
        self.stream_queries.fetch_add(1, Ordering::SeqCst);
        let record = |cursor| MonitoringEventRecord {
            cursor,
            event: MonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "PROJECTION".to_owned(),
                entity_type: "PLAN".to_owned(),
                entity_id: None,
                payload: serde_json::json!({"direction": "BUY"}),
            },
        };
        Ok(match cursor {
            0 => vec![record(1)],
            10 => vec![record(11), record(12), record(13)],
            _ => Vec::new(),
        })
    }
}

#[async_trait]
impl SyncStore for FakeSync {
    async fn load(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        Ok(Some(self.status.clone()))
    }

    async fn mark_recent_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_recent_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
        _latest: i64,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_backfill_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_backfill_progress(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_backfill_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_failed(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _operation: SyncOperation,
        _message: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn all_for_symbol(&self, _symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError> {
        Ok(vec![self.status.clone()])
    }
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn candle(open_time: i64) -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        Decimal::new(410050, 2),
    )
    .unwrap()
}

fn state(capacity: usize) -> (AppState, Arc<FakeCandles>, Arc<MarketHub>) {
    state_with_candles(capacity, vec![candle(60_000), candle(120_000)])
}

fn state_with_candles(
    capacity: usize,
    rows: Vec<Candle>,
) -> (AppState, Arc<FakeCandles>, Arc<MarketHub>) {
    let market = Market::new(
        symbol(),
        true,
        MarketStatus::Trading,
        Decimal::new(1, 2),
        0,
        "COMMODITY",
    )
    .unwrap();
    let candles = Arc::new(FakeCandles {
        fail: AtomicBool::new(false),
        last_limit: AtomicI64::new(0),
        rows,
    });
    let hub = Arc::new(MarketHub::new(capacity));
    let status = SyncStatus {
        symbol: symbol(),
        timeframe: Timeframe::M1,
        oldest_synced_time: Some(60_000),
        latest_synced_time: Some(120_000),
        full_history_complete: false,
        recent_sync_status: SyncRunStatus::Complete,
        backfill_status: SyncRunStatus::Running,
        last_error: None,
    };
    (
        AppState::new(
            Arc::new(FakeMarkets { market }),
            candles.clone(),
            Arc::new(FakeSync { status }),
            hub.clone(),
            1_000,
        ),
        candles,
        hub,
    )
}

fn analysis_store() -> Arc<FakeSignals> {
    let release_id = Uuid::now_v7();
    Arc::new(FakeSignals {
        fail: AtomicBool::new(false),
        stream_queries: AtomicUsize::new(0),
        projection: CurrentPlanProjection {
            symbol: symbol(),
            plan: PlanKind::Rsi,
            target_timeframe: Timeframe::M1,
            readiness: Readiness::Ready,
            setup_state: SetupState::EntryAvailable,
            direction: Direction::Buy,
            trade_state: TradeState::NotEntered,
            tracking_health: TrackingHealth::Live,
            active_signal_id: None,
            active_virtual_trade_id: None,
            version: VersionIdentity::new(release_id, "rsi-v1", "rsi-1m-v1").unwrap(),
            evaluated_closed_candle_time: 239_999,
        },
        signal: SignalDefinition {
            signal_id: Uuid::now_v7(),
            symbol: symbol(),
            plan: PlanKind::Rsi,
            target_timeframe: Timeframe::M1,
            evaluated_at: 239_999,
            direction: Direction::Buy,
            execution_instruction: ExecutionInstruction::NextOpen,
            entry_zone: None,
            projected_entry: Some(Decimal::new(4_100, 0)),
            stop_loss: Decimal::new(4_080, 0),
            primary_target: Decimal::new(4_140, 0),
            execution_plan: None,
            reference_levels: Vec::new(),
            risk_reward: Decimal::TWO,
            pre_entry_validation: None,
            confidence: Decimal::ONE,
            evidence: Vec::new(),
            effective_at: 240_000,
            detected_at: 240_000,
            published_at: 240_000,
            release_id,
            algorithm_version: "rsi-v1".to_owned(),
            parameter_version: "rsi-1m-v1".to_owned(),
            created_at: 240_000,
            expires_at: None,
            htf_context: None,
        },
    })
}

async fn json_response(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn next_sse_event_name(body: &mut Body) -> String {
    let frame = timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix("event: "))
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn markets_candles_and_sync_status_have_stable_json_contracts() {
    let (state, candle_store, _) = state(16);
    let app = api::router(state);

    let (status, markets) = json_response(app.clone(), "/api/v1/markets").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(markets[0]["symbol"], "XAUUSDT");
    assert_eq!(markets[0]["intervals"].as_array().unwrap().len(), 7);

    let (status, candles) =
        json_response(app.clone(), "/api/v1/candles?symbol=XAUUSDT&interval=1m").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(candles["data"][0]["open"], "4100.00");
    assert_eq!(candles["nextBefore"], 60_000);
    assert_eq!(candle_store.last_limit.load(Ordering::SeqCst), 1_000);

    let (_, before) = json_response(
        app.clone(),
        "/api/v1/candles?symbol=XAUUSDT&interval=1m&before=120000",
    )
    .await;
    assert_eq!(before["data"].as_array().unwrap().len(), 1);

    let (status, sync) = json_response(app, "/api/v1/sync-status?symbol=XAUUSDT").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sync[0]["recentSyncStatus"], "complete");
    assert_eq!(sync[0]["backfillStatus"], "running");
}

#[tokio::test]
async fn invalid_queries_are_400_and_storage_failure_is_503() {
    let (state, candles, _) = state(16);
    let app = api::router(state);
    for uri in [
        "/api/v1/candles?symbol=xauusdt&interval=1m",
        "/api/v1/candles?symbol=XAUUSDT&interval=2m",
        "/api/v1/candles?symbol=XAUUSDT&interval=1m&limit=0",
        "/api/v1/candles?symbol=XAUUSDT&interval=1m&limit=5001",
    ] {
        let (status, body) = json_response(app.clone(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");
    }

    candles.fail.store(true, Ordering::SeqCst);
    let (status, body) = json_response(app, "/api/v1/candles?symbol=XAUUSDT&interval=1m").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "storage_unavailable");
}

#[tokio::test]
async fn analysis_projection_history_and_ordered_stream_have_stable_contracts() {
    let (state, _, _) = state(16);
    let store = analysis_store();
    let app = api::router(state.with_analysis(store.clone()));
    let query = "symbol=XAUUSDT&interval=1m&plan=RSI";

    let (status, projection) =
        json_response(app.clone(), &format!("/api/v1/analysis/latest?{query}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(projection["direction"], "BUY");

    let (status, signals) =
        json_response(app.clone(), &format!("/api/v1/signals?{query}&limit=10")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(signals["items"][0]["projectedEntry"], "4100");

    let (status, invalid) = json_response(
        app.clone(),
        "/api/v1/signals?symbol=xau&interval=2m&plan=BAD",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["code"], "invalid_request");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/analysis-stream?cursor=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let first = timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8(first.to_vec()).unwrap();
    assert!(text.contains("id: 1"));
    assert!(text.contains("event: projection"));

    store.fail.store(true, Ordering::SeqCst);
    let (status, body) = json_response(app, &format!("/api/v1/analysis/latest?{query}")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "analysis_unavailable");
}

#[tokio::test]
async fn analysis_stream_sends_a_fetched_batch_before_querying_again() {
    let (state, _, _) = state(16);
    let store = analysis_store();
    let app = api::router(state.with_analysis(store.clone()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/analysis-stream?cursor=0")
                .header("last-event-id", "10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let mut text = String::new();
    while !text.contains("id: 13") {
        let chunk = timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(text.contains("id: 11") && text.contains("id: 12"));
    assert_eq!(store.stream_queries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rsi_market_metrics_return_all_timeframes() {
    let rows = (0..20)
        .map(|index| {
            Candle::new(
                symbol(),
                Timeframe::M1,
                i64::from(index) * 60_000,
                i64::from(index) * 60_000 + 59_999,
                Decimal::new(4_100 + i64::from(index), 0),
                Decimal::new(4_102 + i64::from(index), 0),
                Decimal::new(4_098 + i64::from(index), 0),
                Decimal::new(4_101 + i64::from(index), 0),
            )
            .unwrap()
        })
        .collect();
    let (state, _, _) = state_with_candles(16, rows);
    let app = api::router(state);

    let (status, body) = json_response(app, "/api/v1/market-metrics/rsi?symbol=XAUUSDT").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 7);
    assert_eq!(body[0]["timeframe"], "1m");
    assert_eq!(body[0]["value"], "100");
}

#[tokio::test]
async fn range_forecast_without_a_model_says_so_instead_of_failing() {
    let (state, _, _) = state(16);
    let app = api::router(state);

    let (status, body) =
        json_response(app, "/api/v1/market-metrics/range-forecast?symbol=XAUUSDT").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["symbol"], "XAUUSDT");
    assert_eq!(body["horizonBars"], 24);
    assert_eq!(body["range"], Value::Null);
    assert_eq!(body["reason"], "range forecast model is not configured");
}

#[tokio::test]
async fn support_resistance_endpoint_uses_wide_default_lookback_and_validates_query() {
    let rows = (0..120)
        .map(|index| candle(i64::from(index) * 60_000))
        .collect();
    let (state, candle_store, _) = state_with_candles(16, rows);
    let app = api::router(state);

    let (status, body) = json_response(
        app.clone(),
        "/api/v1/support-resistance?symbol=XAUUSDT&interval=1m",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().is_some());
    assert_eq!(candle_store.last_limit.load(Ordering::SeqCst), 2_000);

    let (status, body) = json_response(
        app,
        "/api/v1/support-resistance?symbol=XAUUSDT&interval=1m&lookback=99",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn market_stream_starts_with_snapshot_and_lag_emits_resync() {
    let (state, _, hub) = state(1);
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    let app = api::router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/market-stream?symbol=XAUUSDT&interval=1m")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body();
    assert_eq!(next_sse_event_name(&mut body).await, "snapshot");

    hub.set_status(&symbol(), LiveStatus::Degraded).await;
    assert_eq!(next_sse_event_name(&mut body).await, "status");
    hub.apply_forming(candle(120_000)).await;
    assert_eq!(next_sse_event_name(&mut body).await, "forming");
    hub.apply_closed(candle(120_000)).await;
    assert_eq!(next_sse_event_name(&mut body).await, "closed");

    hub.apply_forming(candle(180_000)).await;
    hub.apply_forming(candle(240_000)).await;
    assert_eq!(next_sse_event_name(&mut body).await, "resync");
}
