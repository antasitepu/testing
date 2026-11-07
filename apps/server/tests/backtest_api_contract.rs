use analysis_domain::{
    MaterializedProfile, PlanKind, PlanParameters, PlanProfileKey, ReleaseMode,
    StructuredAnalysisError, TrendBreakoutParameters, VersionIdentity,
};
use analysis_service::{
    AnalysisServiceError, BacktestRunInput, BacktestRunRecord, BacktestRunResult,
    BacktestRunStatus, BacktestRunView, BacktestStore, ConfigurationRelease, ConfigurationStore,
    ValidationGate,
};
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use gold_monitoring_server::{api, state::AppState};
use http_body_util::BodyExt;
use market_domain::{Candle, Market, MarketStatus, Symbol, SyncRunStatus, SyncStatus, Timeframe};
use market_service::{
    CandleStore, MarketHub, MarketStore, ServiceError, SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn input(release_id: Uuid) -> BacktestRunInput {
    BacktestRunInput {
        symbol: symbol(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: Timeframe::M1,
        start_time: 0,
        end_time: 60_000,
        release_id,
        algorithm_version: "trend-breakout-v1".to_owned(),
        parameter_version: "trend-breakout-1m-v1".to_owned(),
        fees: analysis_service::FeeConfig::default(),
        slippage: analysis_service::SlippageConfig::default(),
        account: None,
        code_commit_hash: "abc1234".to_owned(),
    }
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
        versions: VersionIdentity::new(release_id, "trend-breakout-v1", "trend-breakout-1m-v1")
            .unwrap(),
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

#[derive(Default)]
struct FakeBacktests {
    runs: Mutex<HashMap<Uuid, BacktestRunView>>,
}

#[async_trait]
impl BacktestStore for FakeBacktests {
    async fn create_run(&self, record: BacktestRunRecord) -> Result<Uuid, AnalysisServiceError> {
        let view = BacktestRunView {
            run_id: record.run_id,
            status: BacktestRunStatus::Pending,
            input: record.input,
            metrics: None,
            error: None,
            trades: Vec::new(),
        };
        self.runs.lock().unwrap().insert(record.run_id, view);
        Ok(record.run_id)
    }

    async fn complete_run(&self, _result: BacktestRunResult) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn fail_run(
        &self,
        _run_id: Uuid,
        _error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn run(&self, run_id: Uuid) -> Result<Option<BacktestRunView>, AnalysisServiceError> {
        Ok(self.runs.lock().unwrap().get(&run_id).cloned())
    }
}

struct FakeConfigurations {
    profiles: Vec<MaterializedProfile>,
}

#[async_trait]
impl ConfigurationStore for FakeConfigurations {
    async fn create_draft(
        &self,
        _release: &ConfigurationRelease,
    ) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn activate(
        &self,
        _release_id: Uuid,
        _gate: &ValidationGate,
    ) -> Result<(), AnalysisServiceError> {
        unreachable!()
    }

    async fn active_profiles(
        &self,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }

    async fn release_mode(&self, _release_id: Uuid) -> Result<ReleaseMode, AnalysisServiceError> {
        Ok(ReleaseMode::Research)
    }

    async fn profiles_for_release(
        &self,
        _release_id: Uuid,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }
}

struct EmptyMarkets;
struct EmptyCandles;
struct EmptySync;

#[async_trait]
impl MarketStore for EmptyMarkets {
    async fn upsert_market(&self, _market: &Market) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn load_market(&self, _symbol: &Symbol) -> Result<Option<Market>, ServiceError> {
        Ok(Some(
            Market::new(
                symbol(),
                true,
                MarketStatus::Trading,
                Decimal::new(1, 2),
                0,
                "COMMODITY",
            )
            .unwrap(),
        ))
    }

    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl CandleStore for EmptyCandles {
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
        _before: Option<i64>,
        _limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        Ok(Vec::new())
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

#[async_trait]
impl SyncStore for EmptySync {
    async fn load(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        Ok(None)
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
        Ok(vec![SyncStatus {
            symbol: symbol(),
            timeframe: Timeframe::M1,
            oldest_synced_time: None,
            latest_synced_time: None,
            full_history_complete: false,
            recent_sync_status: SyncRunStatus::Complete,
            backfill_status: SyncRunStatus::Running,
            last_error: None,
        }])
    }
}

fn build_app(profiles: Vec<MaterializedProfile>, backtests: Arc<FakeBacktests>) -> axum::Router {
    api::router(
        AppState::new(
            Arc::new(EmptyMarkets),
            Arc::new(EmptyCandles),
            Arc::new(EmptySync),
            Arc::new(MarketHub::new(16)),
            1_000,
        )
        .with_backtests(backtests, Arc::new(FakeConfigurations { profiles })),
    )
}

async fn request(app: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

#[tokio::test]
async fn post_returns_run_id_and_get_returns_immutable_pending_run() {
    let release_id = Uuid::from_u128(1);
    let backtests = Arc::new(FakeBacktests::default());
    let app = build_app(vec![profile(release_id)], backtests);
    let body = serde_json::to_vec(&input(release_id)).unwrap();

    let (status, created) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/v1/backtests")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run_id = created["runId"].as_str().unwrap();

    let (status, run) = request(
        app,
        Request::builder()
            .uri(format!("/api/v1/backtests/{run_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"], "PENDING");
    assert_eq!(run["input"]["codeCommitHash"], "abc1234");
    assert_eq!(run["trades"], json!([]));
}

#[tokio::test]
async fn invalid_range_or_version_returns_400_and_missing_release_returns_404() {
    let release_id = Uuid::from_u128(2);
    let backtests = Arc::new(FakeBacktests::default());
    let app = build_app(vec![profile(release_id)], backtests);

    let mut invalid = input(release_id);
    invalid.end_time = invalid.start_time - 1;
    let (status, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/v1/backtests")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&invalid).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let mut invalid = input(release_id);
    invalid.algorithm_version = "wrong-version".to_owned();
    let (status, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/v1/backtests")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&invalid).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let missing_app = build_app(Vec::new(), Arc::new(FakeBacktests::default()));
    let (status, body) = request(
        missing_app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/backtests")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&input(release_id)).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "release_not_found");
}

#[tokio::test]
async fn unknown_run_returns_404() {
    let app = build_app(
        vec![profile(Uuid::from_u128(3))],
        Arc::new(FakeBacktests::default()),
    );
    let (status, body) = request(
        app,
        Request::builder()
            .uri(format!("/api/v1/backtests/{}", Uuid::from_u128(999)))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "backtest_not_found");
}
