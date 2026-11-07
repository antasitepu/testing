use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use binance_adapter::{BinanceError, BinanceRestClient};
use market_domain::{MarketStatus, SessionStatus, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Clone)]
struct FixtureState {
    exchange_info: Value,
    klines: Value,
    aggregate_trades: Value,
    trading_schedule: Value,
    requests: Arc<Mutex<Vec<String>>>,
}

struct FixtureServer {
    base_url: Url,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fixture(path: &str) -> Value {
    serde_json::from_str(match path {
        "exchange_info" => include_str!("fixtures/exchange_info.json"),
        "klines" => include_str!("fixtures/klines.json"),
        "agg_trades" => include_str!("fixtures/agg_trades.json"),
        "trading_schedule" => include_str!("fixtures/trading_schedule.json"),
        _ => panic!("unknown fixture"),
    })
    .unwrap()
}

async fn spawn_server(mut state: FixtureState) -> FixtureServer {
    state.requests = Arc::new(Mutex::new(Vec::new()));
    let requests = state.requests.clone();
    let app = Router::new()
        .fallback(get(fixture_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FixtureServer {
        base_url: Url::parse(&format!("http://{address}/")).unwrap(),
        requests,
        task,
    }
}

fn default_state() -> FixtureState {
    FixtureState {
        exchange_info: fixture("exchange_info"),
        klines: fixture("klines"),
        aggregate_trades: fixture("agg_trades"),
        trading_schedule: fixture("trading_schedule"),
        requests: Arc::new(Mutex::new(Vec::new())),
    }
}

async fn fixture_handler(
    State(state): State<FixtureState>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    state.requests.lock().unwrap().push(
        uri.path_and_query()
            .map_or_else(|| uri.path().to_owned(), ToString::to_string),
    );
    let body = match uri.path() {
        "/fapi/v1/time" => json!({"serverTime": 1782151404000_i64}),
        "/fapi/v1/exchangeInfo" => state.exchange_info,
        "/fapi/v1/klines" => state.klines,
        "/fapi/v1/aggTrades" => state.aggregate_trades,
        "/fapi/v1/tradingSchedule" => state.trading_schedule,
        _ => return (StatusCode::NOT_FOUND, "missing fixture").into_response(),
    };
    Json(body).into_response()
}

fn client(server: &FixtureServer) -> BinanceRestClient {
    BinanceRestClient::new(reqwest::Client::new(), server.base_url.clone())
}

#[tokio::test]
async fn maps_official_rest_shapes_and_preserves_query_contracts() {
    let server = spawn_server(default_state()).await;
    let client = client(&server);
    let symbol = Symbol::new("XAUUSDT").unwrap();

    assert_eq!(client.server_time().await.unwrap(), 1_782_151_404_000);
    let market = client.market(&symbol).await.unwrap();
    assert_eq!(market.status, MarketStatus::Trading);
    assert_eq!(market.price_tick, Decimal::new(1, 2));
    assert_eq!(market.session_market, "COMMODITY");

    let candles = client
        .klines(&symbol, Timeframe::M1, None, None, 1_500)
        .await
        .unwrap();
    assert_eq!(candles.len(), 1);
    assert_eq!(candles[0].open_time, 1_782_151_320_000);
    assert_eq!(candles[0].close, Decimal::new(418686, 2));

    let trades = client
        .aggregate_trades_from(&symbol, 42, 1_000)
        .await
        .unwrap();
    assert_eq!(trades[0].aggregate_trade_id, 49_875_228);
    assert_eq!(trades[0].first_trade_id, 95_845_346);
    assert_eq!(trades[0].last_trade_id, 95_845_346);
    assert_eq!(trades[0].price, Decimal::new(418714, 2));

    let sessions = client.trading_schedule("COMMODITY").await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].status, SessionStatus::Regular);
    assert_eq!(sessions[1].status, SessionStatus::NoTrading);

    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests[0], "/fapi/v1/time");
    assert_eq!(requests[1], "/fapi/v1/exchangeInfo");
    assert!(requests[2].starts_with("/fapi/v1/klines?symbol=XAUUSDT&interval=1m&limit=1500"));
    assert!(requests[3].starts_with("/fapi/v1/aggTrades?symbol=XAUUSDT&fromId=42&limit=1000"));
    assert_eq!(requests[4], "/fapi/v1/tradingSchedule");
}

#[tokio::test]
async fn malformed_payloads_return_typed_errors() {
    let default = default_state();
    let missing_symbol_server = spawn_server(default.clone()).await;
    let error = client(&missing_symbol_server)
        .market(&Symbol::new("BTCUSDT").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, BinanceError::InvalidPayload { .. }));

    let no_filter_server = spawn_server(FixtureState {
        exchange_info: json!({
            "symbols": [{
                "symbol": "XAUUSDT",
                "contractType": "TRADIFI_PERPETUAL",
                "onboardDate": 1765440300000_i64,
                "status": "TRADING",
                "underlyingType": "COMMODITY",
                "filters": []
            }]
        }),
        ..default.clone()
    })
    .await;
    assert!(matches!(
        client(&no_filter_server)
            .market(&Symbol::new("XAUUSDT").unwrap())
            .await,
        Err(BinanceError::InvalidPayload { .. })
    ));

    let bad_decimal_server = spawn_server(FixtureState {
        exchange_info: {
            let mut payload = fixture("exchange_info");
            payload["symbols"][0]["filters"][0]["tickSize"] = json!("bad");
            payload
        },
        ..default.clone()
    })
    .await;
    assert!(matches!(
        client(&bad_decimal_server)
            .market(&Symbol::new("XAUUSDT").unwrap())
            .await,
        Err(BinanceError::InvalidPayload { .. })
    ));

    let malformed_kline_server = spawn_server(FixtureState {
        klines: json!([[1782151320000_i64, "4187.40"]]),
        ..default
    })
    .await;
    assert!(matches!(
        client(&malformed_kline_server)
            .klines(
                &Symbol::new("XAUUSDT").unwrap(),
                Timeframe::M1,
                None,
                None,
                100
            )
            .await,
        Err(BinanceError::InvalidPayload { .. })
    ));
}

#[tokio::test]
async fn invalid_limits_are_rejected_before_network_io() {
    let server = spawn_server(default_state()).await;
    let client = client(&server);
    let symbol = Symbol::new("XAUUSDT").unwrap();
    assert!(matches!(
        client.klines(&symbol, Timeframe::M1, None, None, 0).await,
        Err(BinanceError::InvalidPayload { .. })
    ));
    assert!(matches!(
        client.aggregate_trades_from(&symbol, 0, 1_001).await,
        Err(BinanceError::InvalidPayload { .. })
    ));
    assert!(server.requests.lock().unwrap().is_empty());
}
