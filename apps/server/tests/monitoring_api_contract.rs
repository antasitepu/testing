use analysis_domain::{
    MaterializedProfile, PlanKind, PlanParameters, PlanProfileKey, ReleaseMode, RsiPlanParameters,
    StructuredAnalysisError, VersionIdentity,
};
use analysis_service::{
    AnalysisServiceError, BacktestRunRecord, BacktestRunResult, BacktestRunView, BacktestStore,
    ConfigurationRelease, ConfigurationStore, RejectedCandidateStore, RejectedPlanCandidate,
    RejectedPlanCandidateQuery, ValidationGate,
};
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use execution_service::{
    ExecutionError, MonitoringEventEnvelope, MonitoringSnapshot, MonitoringStore,
    NewScalpingAuditSnapshot, OutboxStreamBatch, ScalpingAuditSnapshot, ScalpingAuditSnapshotPage,
    ScalpingAuditSnapshotQuery, ScalpingAuditStore, TrackingHealth, VirtualTradeDetail,
    VirtualTradeHistoryPage, VirtualTradeHistoryQuery, VirtualTradeStore,
};
use gold_monitoring_server::{api, state::AppState};
use http_body_util::BodyExt;
use market_domain::{Candle, Market, MarketStatus, Symbol, SyncStatus, Timeframe};
use market_service::{
    CandleStore, MarketHub, MarketStore, ServiceError, SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Default)]
struct FakeMonitoring {
    published: Mutex<Vec<i64>>,
}

#[async_trait]
impl MonitoringStore for FakeMonitoring {
    async fn snapshot(&self) -> Result<MonitoringSnapshot, ExecutionError> {
        Ok(MonitoringSnapshot {
            recommendations: vec![json!({
                "signalId": Uuid::now_v7(),
                "symbol": "XAUUSDT",
                "plan": "RSI",
                "timeframe": "1m",
                "direction": "BUY",
                "entryLow": "4100.00",
                "entryHigh": "4110.00",
                "primaryTarget": "4140.00",
                "stopLoss": "4080.00"
            })],
            virtual_trades: vec![json!({
                "virtualTradeId": Uuid::now_v7(),
                "signalId": Uuid::now_v7(),
                "symbol": "XAUUSDT",
                "plan": "RSI",
                "timeframe": "1m",
                "direction": "BUY",
                "tradeState": "OPEN",
                "actualEntryTime": 1_782_290_700_000_i64,
                "actualEntryPrice": "4105.00",
                "primaryTarget": "4140.00",
                "analyticalTarget": "4150.00",
                "positionStage": "RUNNER",
                "remainingFraction": "0.5",
                "realizedFraction": "0.5",
                "realizedR": "0.5",
                "tradeHealth": "WEAKENING",
                "thesisInvalidation": "4092.00",
                "hardStop": "4085.00",
                "activeStop": "4105.00",
                "tp1Price": "4110.00",
                "tp1Fraction": "0.5",
                "tp1Filled": true,
                "tp1FilledAt": 1_782_291_000_000_i64,
                "tp2Zone": {"low": "4148.00", "high": "4150.00"},
                "tp2ExecutionEdge": "4148.00",
                "ambiguity": "AMBIGUOUS",
                "reconciliationSource": "OHLC_RECONCILED"
            })],
            summaries: vec![json!({"plan": "ICT", "condition": "BUY"})],
            health: BTreeMap::from([("XAUUSDT".to_owned(), TrackingHealth::Live)]),
            latest_cursor: 7,
        })
    }

    async fn events_after(
        &self,
        cursor: i64,
        _limit: u32,
    ) -> Result<OutboxStreamBatch, ExecutionError> {
        if cursor < 0 {
            return Ok(OutboxStreamBatch::resync_required(cursor));
        }
        Ok(OutboxStreamBatch::from_events(
            cursor,
            vec![MonitoringEventEnvelope {
                cursor: cursor + 1,
                event_id: Uuid::now_v7(),
                event_type: "virtual-trade-opened".to_owned(),
                entity_type: "virtual_trade".to_owned(),
                entity_id: None,
                payload: json!({
                    "virtualTradeId": Uuid::now_v7(),
                    "signalId": Uuid::now_v7(),
                    "symbol": "XAUUSDT",
                    "plan": "RSI",
                    "timeframe": "1m",
                    "direction": "BUY",
                    "price": "4105.00"
                }),
            }],
        ))
    }

    async fn mark_published(&self, cursor: i64) -> Result<(), ExecutionError> {
        self.published.lock().unwrap().push(cursor);
        Ok(())
    }

    async fn cleanup_published_before(
        &self,
        _older_than_ms: i64,
        _limit: u32,
    ) -> Result<u64, ExecutionError> {
        Ok(0)
    }
}

struct EmptyMarkets;
struct EmptyCandles;
struct EmptySync;
struct UnusedBacktests;
struct FakeVirtualTrades;
struct FakeRejectedCandidates {
    items: Vec<RejectedPlanCandidate>,
}
#[derive(Default)]
struct FakeScalpingAudits {
    snapshots: Mutex<Vec<ScalpingAuditSnapshot>>,
}

struct FakeConfigurations {
    profiles: Vec<MaterializedProfile>,
}

#[async_trait]
impl BacktestStore for UnusedBacktests {
    async fn create_run(&self, _input: BacktestRunRecord) -> Result<Uuid, AnalysisServiceError> {
        Err(AnalysisServiceError::NotFound("unused".to_owned()))
    }

    async fn complete_run(&self, _result: BacktestRunResult) -> Result<(), AnalysisServiceError> {
        Ok(())
    }

    async fn fail_run(
        &self,
        _run_id: Uuid,
        _error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }

    async fn run(&self, _run_id: Uuid) -> Result<Option<BacktestRunView>, AnalysisServiceError> {
        Ok(None)
    }
}

#[async_trait]
impl VirtualTradeStore for FakeVirtualTrades {
    async fn history(
        &self,
        query: VirtualTradeHistoryQuery,
    ) -> Result<VirtualTradeHistoryPage, ExecutionError> {
        assert_eq!(query.symbol.as_deref(), Some("XAUUSDT"));
        assert!(matches!(
            query.timeframe.as_deref(),
            Some("1m" | "5m" | "15m")
        ));
        Ok(VirtualTradeHistoryPage {
            items: vec![json!({
                "virtualTradeId": Uuid::now_v7(),
                "signalId": Uuid::now_v7(),
                "symbol": "XAUUSDT",
                "plan": "RSI",
                "timeframe": query.timeframe.clone().unwrap_or_else(|| "1m".to_owned()),
                "interval": query.timeframe.clone().unwrap_or_else(|| "1m".to_owned()),
                "direction": "SELL",
                "tradeState": "CLOSED",
                "actualEntryTime": 1_782_290_700_000_i64,
                "actualEntryPrice": "4105.00",
                "exitTime": 1_782_291_300_000_i64,
                "exitPrice": "4095.00",
                "exitReason": "target touched",
                "grossR": "1.2"
            })],
            next_cursor: None,
        })
    }

    async fn detail(&self, _trade_id: Uuid) -> Result<Option<VirtualTradeDetail>, ExecutionError> {
        Ok(None)
    }
}

#[async_trait]
impl RejectedCandidateStore for FakeRejectedCandidates {
    async fn recent_rejections(
        &self,
        query: RejectedPlanCandidateQuery,
    ) -> Result<Vec<RejectedPlanCandidate>, AnalysisServiceError> {
        assert_eq!(query.symbol.as_str(), "XAUUSDT");
        assert_eq!(
            query.timeframes,
            vec![Timeframe::M1, Timeframe::M5, Timeframe::M15]
        );
        assert_eq!(query.limit, 50);
        Ok(self.items.clone())
    }
}

#[async_trait]
impl ScalpingAuditStore for FakeScalpingAudits {
    async fn create(
        &self,
        snapshot: NewScalpingAuditSnapshot,
    ) -> Result<ScalpingAuditSnapshot, ExecutionError> {
        let saved = ScalpingAuditSnapshot {
            snapshot_id: Uuid::now_v7(),
            symbol: snapshot.symbol,
            mode: snapshot.mode,
            timeframes: snapshot.timeframes,
            report: snapshot.report,
            journal: snapshot.journal,
            audit: snapshot.audit,
            created_at_ms: 1_782_360_000_000,
        };
        self.snapshots.lock().unwrap().push(saved.clone());
        Ok(saved)
    }

    async fn list(
        &self,
        query: ScalpingAuditSnapshotQuery,
    ) -> Result<ScalpingAuditSnapshotPage, ExecutionError> {
        assert_eq!(query.symbol.as_deref(), Some("XAUUSDT"));
        assert!(query.limit <= 10);
        let mut items = self.snapshots.lock().unwrap().clone();
        items.sort_by(|left, right| right.created_at_ms.cmp(&left.created_at_ms));
        if let Some(cursor) = query.cursor {
            let cursor: i64 = cursor.parse().unwrap();
            items.retain(|snapshot| snapshot.created_at_ms < cursor);
        }
        let has_more = items.len() > query.limit as usize;
        items.truncate(query.limit as usize);
        let next_cursor = has_more
            .then(|| {
                items
                    .last()
                    .map(|snapshot| snapshot.created_at_ms.to_string())
            })
            .flatten();
        Ok(ScalpingAuditSnapshotPage { items, next_cursor })
    }
}

#[async_trait]
impl ConfigurationStore for FakeConfigurations {
    async fn create_draft(
        &self,
        _release: &ConfigurationRelease,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }

    async fn activate(
        &self,
        _release_id: Uuid,
        _gate: &ValidationGate,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }

    async fn active_profiles(
        &self,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }

    async fn release_mode(&self, _release_id: Uuid) -> Result<ReleaseMode, AnalysisServiceError> {
        Ok(ReleaseMode::PaperActive)
    }

    async fn profiles_for_release(
        &self,
        _release_id: Uuid,
        _symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        Ok(self.profiles.clone())
    }
}

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
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn monitoring_snapshot_has_complete_contract() {
    let monitoring = Arc::new(FakeMonitoring::default());
    let app = api::router(state().with_monitoring(monitoring));

    let (status, body) = json_response(app, "/api/v1/monitoring/snapshot").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["latestCursor"], 7);
    assert_eq!(body["recommendations"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["virtualTrades"][0]["actualEntryTime"],
        1_782_290_700_000_i64
    );
    assert_eq!(body["virtualTrades"][0]["actualEntryPrice"], "4105.00");
    assert_eq!(body["health"]["XAUUSDT"], "LIVE");
}

#[tokio::test]
async fn monitoring_stream_replays_cursor_marks_published_and_resyncs_old_cursor() {
    let monitoring = Arc::new(FakeMonitoring::default());
    let app = api::router(state().with_monitoring(monitoring.clone()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/monitoring-stream?after=7")
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
    let first = timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = std::str::from_utf8(first.data_ref().unwrap()).unwrap();
    assert!(first.contains("id: 8"));
    assert!(first.contains("event: virtual-trade-opened"));
    assert_eq!(*monitoring.published.lock().unwrap(), vec![8]);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/monitoring-stream?after=-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    let first = timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = std::str::from_utf8(first.data_ref().unwrap()).unwrap();
    assert!(first.contains("event: resync-required"));
}

#[tokio::test]
async fn monitoring_stream_reconnect_resumes_from_last_event_id() {
    let monitoring = Arc::new(FakeMonitoring::default());
    let app = api::router(state().with_monitoring(monitoring.clone()));

    // EventSource reconnects with its original URL plus Last-Event-ID.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/monitoring-stream?after=7")
                .header("last-event-id", "20")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    let first = timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = std::str::from_utf8(first.data_ref().unwrap()).unwrap();
    assert!(first.contains("id: 21"));
    assert_eq!(*monitoring.published.lock().unwrap(), vec![21]);
}

#[tokio::test]
async fn runtime_strategy_summary_joins_profiles_and_monitoring_state() {
    let release_id = Uuid::now_v7();
    let profile = MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Rsi,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(release_id, "RSI-runtime-v1", "RSI-1m-runtime-v3").unwrap(),
        parameters: PlanParameters::Rsi(RsiPlanParameters {
            rsi_period: 14,
            oversold_threshold: Decimal::new(35, 0),
            overbought_threshold: Decimal::new(65, 0),
            divergence_required: false,
            minimum_pivot_distance: 2,
            maximum_pivot_distance: 20,
            confirmed_swing_lookback: 50,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            target_rr: Decimal::new(15, 1),
            max_bars_valid: 10,
            reversal_confirmation_bars: 2,
        }),
        required_timeframes: vec![Timeframe::M5],
        minimum_history_bars: 200,
        warmup_bars: 50,
    };
    let configurations = Arc::new(FakeConfigurations {
        profiles: vec![profile],
    });
    let monitoring = Arc::new(FakeMonitoring::default());
    let app = api::router(
        state()
            .with_backtests(Arc::new(UnusedBacktests), configurations)
            .with_monitoring(monitoring),
    );

    let (status, body) =
        json_response(app, "/api/v1/runtime/strategy-summary?symbol=XAUUSDT").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["symbol"], "XAUUSDT");
    assert_eq!(body["totalProfiles"], 1);
    assert_eq!(body["activeOverlayCount"], 2);
    assert_eq!(body["openTradeCount"], 1);
    assert_eq!(body["decisions"].as_array().unwrap().len(), 2);
    assert_eq!(body["decisions"][0]["action"], "ENTRY");
    assert_eq!(body["decisions"][0]["label"], "entry");
    assert_eq!(body["decisions"][1]["action"], "MANAGE");
    let trade = &body["decisions"][1];
    assert_eq!(trade["positionStage"], "RUNNER");
    assert_eq!(trade["remainingFraction"], "0.5");
    assert_eq!(trade["realizedFraction"], "0.5");
    assert_eq!(trade["realizedR"], "0.5");
    assert_eq!(trade["tradeHealth"], "WEAKENING");
    assert_eq!(trade["thesisInvalidation"], "4092.00");
    assert_eq!(trade["hardStop"], "4085.00");
    assert_eq!(trade["activeStop"], "4105.00");
    assert_eq!(trade["tp1Price"], "4110.00");
    assert_eq!(trade["tp1Fraction"], "0.5");
    assert_eq!(trade["tp1Filled"], true);
    assert_eq!(trade["tp1FilledAt"], 1_782_291_000_000_i64);
    assert_eq!(trade["tp2Zone"]["low"], "4148.00");
    assert_eq!(trade["tp2Zone"]["high"], "4150.00");
    assert_eq!(trade["tp2ExecutionEdge"], "4148.00");
    assert_eq!(trade["analyticalTarget"], "4150.00");
    assert_eq!(trade["ambiguity"], "AMBIGUOUS");
    assert_eq!(trade["reconciliationSource"], "OHLC_RECONCILED");
    assert_eq!(body["rows"][0]["plan"], "RSI");
    assert_eq!(body["rows"][0]["timeframe"], "1m");
    assert_eq!(body["rows"][0]["parameterVersion"], "RSI-1m-runtime-v3");
    assert_eq!(
        body["rows"][0]["candidateLabel"],
        "rsi-os35-ob65-divfalse-rr1.5-atr0.2"
    );
    assert_eq!(body["rows"][0]["tuningStatus"], "LOCKED");
    assert_eq!(body["rows"][0]["requiredTimeframes"][0], "5m");
}

#[tokio::test]
async fn runtime_scalping_dashboard_reports_locked_profiles_journal_and_live_audit() {
    let release_id = Uuid::now_v7();
    let profiles = vec![
        MaterializedProfile {
            key: PlanProfileKey {
                symbol: symbol(),
                plan: PlanKind::Rsi,
                target_timeframe: Timeframe::M1,
            },
            enabled: true,
            profit_protection: analysis_domain::ProfitProtectionParameters::default(),
            position_management: analysis_domain::PositionManagementParameters::default(),
            versions: VersionIdentity::new(release_id, "RSI-runtime-v1", "RSI-1m-runtime-v3")
                .unwrap(),
            parameters: PlanParameters::Rsi(RsiPlanParameters {
                rsi_period: 14,
                oversold_threshold: Decimal::new(35, 0),
                overbought_threshold: Decimal::new(65, 0),
                divergence_required: false,
                minimum_pivot_distance: 2,
                maximum_pivot_distance: 20,
                confirmed_swing_lookback: 50,
                atr_period: 14,
                atr_buffer_multiplier: Decimal::new(2, 1),
                target_rr: Decimal::new(15, 1),
                max_bars_valid: 10,
                reversal_confirmation_bars: 2,
            }),
            required_timeframes: vec![Timeframe::M5],
            minimum_history_bars: 200,
            warmup_bars: 50,
        },
        MaterializedProfile {
            key: PlanProfileKey {
                symbol: symbol(),
                plan: PlanKind::MeanReversion,
                target_timeframe: Timeframe::M5,
            },
            enabled: true,
            profit_protection: analysis_domain::ProfitProtectionParameters::default(),
            position_management: analysis_domain::PositionManagementParameters::default(),
            versions: VersionIdentity::new(
                release_id,
                "MEAN_REVERSION-runtime-v1",
                "MEAN_REVERSION-5m-runtime-v2",
            )
            .unwrap(),
            parameters: PlanParameters::MeanReversion(analysis_domain::MeanReversionParameters {
                zscore_period: 20,
                extreme_zscore: Decimal::new(15, 1),
                bollinger_period: 20,
                bollinger_deviation: Decimal::new(2, 0),
                maximum_bandwidth: Decimal::new(5, 2),
                adx_period: 14,
                maximum_adx: Decimal::new(20, 0),
                atr_period: 14,
                atr_buffer_multiplier: Decimal::new(5, 2),
                minimum_rr: Decimal::new(8, 1),
                max_bars_valid: 10,
            }),
            required_timeframes: vec![Timeframe::M15],
            minimum_history_bars: 200,
            warmup_bars: 50,
        },
    ];
    let configurations = Arc::new(FakeConfigurations { profiles });
    let rejected_candidate_id = Uuid::now_v7();
    let rejections = Arc::new(FakeRejectedCandidates {
        items: vec![RejectedPlanCandidate {
            candidate_id: rejected_candidate_id,
            key: PlanProfileKey {
                symbol: symbol(),
                plan: PlanKind::Smc,
                target_timeframe: Timeframe::M1,
            },
            reason: "missing displacement after sweep".to_owned(),
            payload: json!({"gate": "displacement", "closeTime": 1_782_291_300_000_i64}),
            evaluated_at: 1_782_291_300_000_i64,
        }],
    });
    let app = api::router(
        state()
            .with_backtests(Arc::new(UnusedBacktests), configurations)
            .with_monitoring(Arc::new(FakeMonitoring::default()))
            .with_virtual_trades(Arc::new(FakeVirtualTrades))
            .with_rejected_candidates(rejections),
    );

    let (status, body) =
        json_response(app, "/api/v1/runtime/scalping-dashboard?symbol=XAUUSDT").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "SCALPING");
    assert_eq!(body["timeframes"], json!(["1m", "5m", "15m"]));
    assert_eq!(body["report"]["totalProfiles"], 2);
    assert_eq!(body["report"]["lockedProfiles"], 2);
    assert_eq!(
        body["report"]["rows"][1]["candidateLabel"],
        "mean-z1.5-adx20-bw0.05-rr0.8-atr0.05"
    );
    assert_eq!(body["journal"]["latestCursor"], 7);
    assert_eq!(
        body["journal"]["events"][0]["eventType"],
        "virtual-trade-opened"
    );
    assert_eq!(body["audit"]["rows"][0]["timeframe"], "1m");
    assert_eq!(body["audit"]["rows"][0]["closedTrades"], 1);
    assert_eq!(body["audit"]["rows"][0]["status"], "WATCH");
    assert_eq!(
        body["debug"]["rejectedCandidates"][0]["candidateId"],
        rejected_candidate_id.to_string()
    );
    assert_eq!(body["debug"]["rejectedCandidates"][0]["plan"], "SMC");
    assert_eq!(body["debug"]["rejectedCandidates"][0]["timeframe"], "1m");
    assert_eq!(
        body["debug"]["rejectedCandidates"][0]["reason"],
        "missing displacement after sweep"
    );
    assert_eq!(
        body["debug"]["rejectedCandidates"][0]["payload"]["gate"],
        "displacement"
    );
}

#[tokio::test]
async fn runtime_scalping_audit_snapshots_can_be_created_and_listed() {
    let release_id = Uuid::now_v7();
    let configurations = Arc::new(FakeConfigurations {
        profiles: vec![MaterializedProfile {
            key: PlanProfileKey {
                symbol: symbol(),
                plan: PlanKind::Rsi,
                target_timeframe: Timeframe::M1,
            },
            enabled: true,
            profit_protection: analysis_domain::ProfitProtectionParameters::default(),
            position_management: analysis_domain::PositionManagementParameters::default(),
            versions: VersionIdentity::new(release_id, "RSI-runtime-v1", "RSI-1m-runtime-v3")
                .unwrap(),
            parameters: PlanParameters::Rsi(RsiPlanParameters {
                rsi_period: 14,
                oversold_threshold: Decimal::new(35, 0),
                overbought_threshold: Decimal::new(65, 0),
                divergence_required: false,
                minimum_pivot_distance: 2,
                maximum_pivot_distance: 20,
                confirmed_swing_lookback: 50,
                atr_period: 14,
                atr_buffer_multiplier: Decimal::new(2, 1),
                target_rr: Decimal::new(15, 1),
                max_bars_valid: 10,
                reversal_confirmation_bars: 2,
            }),
            required_timeframes: vec![Timeframe::M5],
            minimum_history_bars: 200,
            warmup_bars: 50,
        }],
    });
    let audits = Arc::new(FakeScalpingAudits::default());
    let app = api::router(
        state()
            .with_backtests(Arc::new(UnusedBacktests), configurations)
            .with_monitoring(Arc::new(FakeMonitoring::default()))
            .with_virtual_trades(Arc::new(FakeVirtualTrades))
            .with_scalping_audits(audits),
    );

    let (status, created) = request_json_response(
        app.clone(),
        "POST",
        "/api/v1/runtime/scalping-audit-snapshots?symbol=XAUUSDT",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["mode"], "SCALPING");
    assert_eq!(created["symbol"], "XAUUSDT");
    assert_eq!(created["report"]["lockedProfiles"], 1);
    assert_eq!(created["audit"]["rows"][0]["status"], "WATCH");

    let (status, listed) = json_response(
        app,
        "/api/v1/runtime/scalping-audit-snapshots?symbol=XAUUSDT&limit=10",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["snapshotId"], created["snapshotId"]);
}

#[tokio::test]
async fn runtime_scalping_audit_comparison_reports_latest_snapshot_deltas() {
    let audits = Arc::new(FakeScalpingAudits::default());
    {
        let mut snapshots = audits.snapshots.lock().unwrap();
        snapshots.push(scalping_snapshot(
            1_782_360_000_000,
            json!({"rows": [{
                "timeframe": "1m",
                "plan": "RSI",
                "status": "WATCH",
                "closedTrades": 8,
                "winRate": 0.25,
                "netR": -1.0,
                "expectancyR": -0.125
            }]}),
        ));
        snapshots.push(scalping_snapshot(
            1_782_363_600_000,
            json!({"rows": [{
                "timeframe": "1m",
                "plan": "RSI",
                "status": "OK",
                "closedTrades": 12,
                "winRate": 0.5,
                "netR": 3.5,
                "expectancyR": 0.2916666667
            }]}),
        ));
    }
    let app = api::router(state().with_scalping_audits(audits));

    let (status, body) = json_response(
        app,
        "/api/v1/runtime/scalping-audit-comparison?symbol=XAUUSDT",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["symbol"], "XAUUSDT");
    assert_eq!(body["rows"][0]["timeframe"], "1m");
    assert_eq!(body["rows"][0]["plan"], "RSI");
    assert_eq!(body["rows"][0]["status"], "OK");
    assert_eq!(body["rows"][0]["previousStatus"], "WATCH");
    assert_eq!(body["rows"][0]["statusChanged"], true);
    assert_eq!(body["rows"][0]["closedTradesDelta"], 4);
    assert_eq!(body["rows"][0]["netRDelta"], 4.5);
    assert!(body["rows"][0]["expectancyRDelta"].as_f64().unwrap() > 0.41);
}

async fn json_response(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    request_json_response(app, "GET", uri).await
}

async fn request_json_response(app: axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn state() -> AppState {
    AppState::new(
        Arc::new(EmptyMarkets),
        Arc::new(EmptyCandles),
        Arc::new(EmptySync),
        Arc::new(MarketHub::new(16)),
        1_000,
    )
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn scalping_snapshot(created_at_ms: i64, audit: Value) -> ScalpingAuditSnapshot {
    ScalpingAuditSnapshot {
        snapshot_id: Uuid::now_v7(),
        symbol: "XAUUSDT".to_owned(),
        mode: "SCALPING".to_owned(),
        timeframes: vec!["1m".to_owned(), "5m".to_owned(), "15m".to_owned()],
        report: json!({"lockedProfiles": 18, "totalProfiles": 18}),
        journal: json!({"latestCursor": 10, "events": []}),
        audit,
        created_at_ms,
    }
}
