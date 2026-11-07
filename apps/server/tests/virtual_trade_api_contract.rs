use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use execution_service::{
    ExecutionError, VirtualTradeDetail, VirtualTradeHistoryPage, VirtualTradeHistoryQuery,
    VirtualTradeStore,
};
use gold_monitoring_server::{api, state::AppState};
use http_body_util::BodyExt;
use market_domain::{Candle, Market, MarketStatus, Symbol, SyncStatus, Timeframe};
use market_service::{
    CandleStore, MarketHub, MarketStore, ServiceError, SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

struct FakeVirtualTrades {
    queries: Mutex<Vec<VirtualTradeHistoryQuery>>,
    trade_id: Uuid,
}

#[async_trait]
impl VirtualTradeStore for FakeVirtualTrades {
    async fn history(
        &self,
        query: VirtualTradeHistoryQuery,
    ) -> Result<VirtualTradeHistoryPage, ExecutionError> {
        self.queries.lock().unwrap().push(query);
        Ok(VirtualTradeHistoryPage {
            items: vec![json!({
                "virtualTradeId": self.trade_id,
                "plan": "ICT",
                "interval": "1m",
                "direction": "BUY",
                "exitReason": "target touched",
                "actualEntryPrice": "4105.00",
                "grossR": "2.500000",
                "positionStage": "RUNNER",
                "remainingFraction": "0.5",
                "realizedFraction": "0.5",
                "realizedR": "0.5",
                "tradeHealth": "HEALTHY",
                "thesisInvalidation": "4092.00",
                "hardStop": "4085.00",
                "activeStop": "4105.00",
                "tp1Price": "4110.00",
                "tp1Fraction": "0.5",
                "tp1Filled": true,
                "tp1FilledAt": 1_782_291_000_000_i64,
                "tp2Zone": {"low": "4148.00", "high": "4150.00"},
                "tp2ExecutionEdge": "4148.00",
                "analyticalTarget": "4150.00",
                "ambiguity": "NONE",
                "reconciliationSource": "EXACT_TRADE"
            })],
            next_cursor: Some(120_000),
        })
    }

    async fn detail(&self, trade_id: Uuid) -> Result<Option<VirtualTradeDetail>, ExecutionError> {
        if trade_id != self.trade_id {
            return Ok(None);
        }
        Ok(Some(VirtualTradeDetail {
            trade: json!({"virtualTradeId": trade_id, "tradeState": "CLOSED"}),
            events: vec![json!({"sequence": 1, "eventType": "ENTRY_TRIGGERED"})],
            signal: json!({"signalId": Uuid::now_v7(), "evidence": []}),
            candle_window: json!({"start": 60_000, "end": 120_000}),
        }))
    }
}

#[tokio::test]
async fn virtual_trade_history_supports_filters_and_cursor_pagination() {
    let store = Arc::new(FakeVirtualTrades {
        queries: Mutex::new(Vec::new()),
        trade_id: Uuid::now_v7(),
    });
    let app = api::router(state().with_virtual_trades(store.clone()));

    let (status, body) = json_response(
        app,
        "/api/v1/virtual-trades?symbol=XAUUSDT&plan=ICT&interval=1m&direction=BUY&exitReason=target%20touched&from=60000&to=120000&cursor=180000&limit=25",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["actualEntryPrice"], "4105.00");
    assert_eq!(body["items"][0]["positionStage"], "RUNNER");
    assert_eq!(body["items"][0]["remainingFraction"], "0.5");
    assert_eq!(body["items"][0]["realizedFraction"], "0.5");
    assert_eq!(body["items"][0]["realizedR"], "0.5");
    assert_eq!(body["items"][0]["tp1Filled"], true);
    assert_eq!(body["items"][0]["tp2Zone"]["low"], "4148.00");
    assert_eq!(body["items"][0]["analyticalTarget"], "4150.00");
    assert_eq!(body["nextCursor"], 120_000);
    let query = store.queries.lock().unwrap()[0].clone();
    assert_eq!(query.plan.as_deref(), Some("ICT"));
    assert_eq!(query.timeframe.as_deref(), Some("1m"));
    assert_eq!(query.direction.as_deref(), Some("BUY"));
    assert_eq!(query.exit_reason.as_deref(), Some("target touched"));
    assert_eq!(query.from, Some(60_000));
    assert_eq!(query.to, Some(120_000));
    assert_eq!(query.cursor, Some(180_000));
    assert_eq!(query.limit, 25);
}

#[tokio::test]
async fn virtual_trade_detail_returns_ordered_events_signal_and_404_for_unknown() {
    let trade_id = Uuid::now_v7();
    let store = Arc::new(FakeVirtualTrades {
        queries: Mutex::new(Vec::new()),
        trade_id,
    });
    let app = api::router(state().with_virtual_trades(store));

    let (status, body) =
        json_response(app.clone(), &format!("/api/v1/virtual-trades/{trade_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["trade"]["tradeState"], "CLOSED");
    assert_eq!(body["events"][0]["sequence"], 1);
    assert!(body["signal"]["evidence"].is_array());
    assert_eq!(body["candleWindow"]["start"], 60_000);

    let (status, body) =
        json_response(app, &format!("/api/v1/virtual-trades/{}", Uuid::now_v7())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
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

fn state() -> AppState {
    AppState::new(
        Arc::new(EmptyMarkets),
        Arc::new(EmptyCandles),
        Arc::new(EmptySync),
        Arc::new(MarketHub::new(16)),
        1_000,
    )
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
                Symbol::new("XAUUSDT").unwrap(),
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
