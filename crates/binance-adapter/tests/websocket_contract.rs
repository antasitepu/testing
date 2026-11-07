use binance_adapter::{
    BinanceError, BinanceWebSocket, combined_market_stream_url, parse_combined_message,
};
use futures_util::StreamExt;
use market_domain::{LiveStatus, MarketEvent, SessionStatus, Symbol, Timeframe};
use rust_decimal::Decimal;
use std::{future::pending, time::Duration};
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::accept_async;
use url::Url;

const PING_INTERVAL: Duration = Duration::from_millis(20);
const IDLE_TIMEOUT: Duration = Duration::from_millis(150);
// Far beyond the idle timeout, so a hang fails the test instead of blocking the suite.
const HANG_LIMIT: Duration = Duration::from_secs(5);

async fn local_server() -> (TcpListener, BinanceWebSocket) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}/", listener.local_addr().unwrap())).unwrap();
    let websocket = BinanceWebSocket::new(url).with_liveness(PING_INTERVAL, IDLE_TIMEOUT);
    (listener, websocket)
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

#[tokio::test]
async fn connection_that_stops_answering_fails_instead_of_waiting_forever() {
    let (listener, websocket) = local_server().await;
    // The peer finishes the handshake and then never reads, so pings go unanswered.
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let _socket = accept_async(tcp).await.unwrap();
        pending::<()>().await;
    });

    let mut stream = websocket.connect(&symbol()).await.unwrap();
    let outcome = timeout(HANG_LIMIT, stream.next())
        .await
        .expect("a dead connection must be reported");

    assert!(matches!(outcome, Some(Err(BinanceError::WebSocket { .. }))));
    server.abort();
}

#[tokio::test]
async fn quiet_connection_that_answers_pings_stays_open() {
    let (listener, websocket) = local_server().await;
    // Reading is what answers pings, and no market data is ever sent. This is what a
    // healthy stream looks like while the market is closed.
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(tcp).await.unwrap();
        while socket.next().await.is_some() {}
    });

    let mut stream = websocket.connect(&symbol()).await.unwrap();

    assert!(
        timeout(IDLE_TIMEOUT * 4, stream.next()).await.is_err(),
        "a quiet but healthy connection must not be dropped"
    );
    server.abort();
}

#[tokio::test]
async fn handshake_that_never_completes_fails_instead_of_waiting_forever() {
    let (listener, websocket) = local_server().await;
    let server = tokio::spawn(async move {
        let (_tcp, _) = listener.accept().await.unwrap();
        pending::<()>().await;
    });

    let outcome = timeout(HANG_LIMIT, websocket.connect(&symbol()))
        .await
        .expect("a stalled handshake must be reported");

    assert!(matches!(outcome, Err(BinanceError::WebSocket { .. })));
    server.abort();
}

#[test]
fn combined_url_contains_all_market_streams_once() {
    let url = combined_market_stream_url(
        &Url::parse("wss://fstream.binance.com/market/").unwrap(),
        &Symbol::new("XAUUSDT").unwrap(),
    )
    .unwrap();
    assert_eq!(
        url.as_str(),
        "wss://fstream.binance.com/market/stream?streams=xauusdt@kline_1m/xauusdt@kline_5m/xauusdt@kline_15m/xauusdt@kline_30m/xauusdt@kline_1h/xauusdt@kline_4h/xauusdt@kline_1d/xauusdt@aggTrade/tradingSession"
    );
}

#[test]
fn parses_forming_and_closed_kline_events() {
    let forming = include_str!("fixtures/kline_event.json");
    let event = parse_combined_message(forming).unwrap().unwrap();
    assert!(matches!(
        event,
        MarketEvent::FormingCandle(candle)
            if candle.timeframe == Timeframe::M1
                && candle.close == Decimal::new(418716, 2)
                && candle.volume == Some(Decimal::new(2505, 1))
                && candle.taker_buy_volume == Some(Decimal::new(15025, 2))
    ));

    let closed = forming.replace("\"x\": false", "\"x\": true");
    assert!(matches!(
        parse_combined_message(&closed).unwrap(),
        Some(MarketEvent::ClosedCandle(_))
    ));
}

#[test]
fn parses_aggregate_trade_and_session_events() {
    assert!(matches!(
        parse_combined_message(include_str!("fixtures/agg_trade_event.json")).unwrap(),
        Some(MarketEvent::AggregateTrade(trade))
            if trade.aggregate_trade_id == 49_875_229
                && trade.first_trade_id == 95_845_347
                && trade.last_trade_id == 95_845_348
                && trade.price == Decimal::new(418713, 2)
    ));
    assert!(matches!(
        parse_combined_message(include_str!("fixtures/trading_session_event.json")).unwrap(),
        Some(MarketEvent::SessionChanged(session))
            if session.session_market == "COMMODITY"
                && session.status == SessionStatus::Regular
    ));
}

#[test]
fn unknown_events_are_ignored_and_invalid_payloads_are_typed_errors() {
    assert_eq!(
        parse_combined_message(r#"{"stream":"other","data":{"e":"unknown"}}"#).unwrap(),
        None
    );
    assert_eq!(
        parse_combined_message(include_str!("fixtures/trading_session_equity_event.json")).unwrap(),
        None
    );
    let invalid = include_str!("fixtures/agg_trade_event.json").replace("4187.13", "bad");
    assert!(matches!(
        parse_combined_message(&invalid),
        Err(BinanceError::InvalidPayload { .. })
    ));
}

#[test]
fn parser_never_creates_status_events_from_wire_payloads() {
    let event =
        parse_combined_message(include_str!("fixtures/trading_session_event.json")).unwrap();
    assert!(!matches!(
        event,
        Some(MarketEvent::StatusChanged {
            status: LiveStatus::Ready,
            ..
        })
    ));
}
