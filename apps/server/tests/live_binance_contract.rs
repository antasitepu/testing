use binance_adapter::{BinanceRestClient, BinanceWebSocket};
use futures_util::StreamExt;
use market_domain::{MarketEvent, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{Row, postgres::PgPoolOptions};
use std::{collections::HashMap, time::Duration};
use tokio::time::timeout;
use url::Url;

const REST_DEFAULT: &str = "https://fapi.binance.com";
const WEBSOCKET_DEFAULT: &str = "wss://fstream.binance.com/market";

fn environment_url(name: &str, default: &str) -> Url {
    dotenvy::dotenv().ok();
    Url::parse(
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .as_deref()
            .unwrap_or(default),
    )
    .unwrap()
}

fn rest_client() -> BinanceRestClient {
    BinanceRestClient::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap(),
        environment_url("BINANCE_REST_BASE_URL", REST_DEFAULT),
    )
}

#[tokio::test]
#[ignore = "requires public Binance network access"]
async fn live_rest_metadata_trade_and_schedule_normalize() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let client = rest_client();
    let market = client.market(&symbol).await.unwrap();
    assert_eq!(market.session_market, "COMMODITY");
    assert!(market.enabled);

    let base = environment_url("BINANCE_REST_BASE_URL", REST_DEFAULT);
    let mut latest_url = base.join("/fapi/v1/aggTrades").unwrap();
    latest_url
        .query_pairs_mut()
        .append_pair("symbol", symbol.as_str())
        .append_pair("limit", "1");
    let latest: Vec<Value> = reqwest::Client::new()
        .get(latest_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let latest_id = latest
        .first()
        .and_then(|trade| trade.get("a"))
        .and_then(Value::as_i64)
        .expect("latest aggregate trade must have an id");
    let normalized = client
        .aggregate_trades_from(&symbol, latest_id, 1)
        .await
        .unwrap();
    assert_eq!(normalized.len(), 1);
    assert!(normalized[0].price > Decimal::ZERO);

    let sessions = client.trading_schedule("COMMODITY").await.unwrap();
    assert!(!sessions.is_empty());
    assert!(
        sessions
            .iter()
            .all(|item| item.session_market == "COMMODITY")
    );
}

#[derive(Clone, Debug)]
struct DatabaseCandle {
    open_time: i64,
    close_time: i64,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
}

#[tokio::test]
#[ignore = "requires synchronized DATABASE_URL and public Binance network access"]
async fn database_samples_match_binance_exactly() {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point to synchronized Phase 1 data");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let client = rest_client();
    let symbol = Symbol::new("XAUUSDT").unwrap();

    for timeframe in Timeframe::ALL {
        let rows = sqlx::query(
            r#"
            WITH selected AS (
                (
                    SELECT open_time, close_time, open, high, low, close
                    FROM candles
                    WHERE symbol = $1 AND interval = $2
                    ORDER BY open_time ASC
                    LIMIT 10
                )
                UNION
                (
                    SELECT open_time, close_time, open, high, low, close
                    FROM candles
                    WHERE symbol = $1 AND interval = $2
                    ORDER BY open_time DESC
                    LIMIT 10
                )
            )
            SELECT open_time, close_time, open, high, low, close
            FROM selected
            ORDER BY open_time ASC
            "#,
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| DatabaseCandle {
            open_time: row.try_get("open_time").unwrap(),
            close_time: row.try_get("close_time").unwrap(),
            open: row.try_get("open").unwrap(),
            high: row.try_get("high").unwrap(),
            low: row.try_get("low").unwrap(),
            close: row.try_get("close").unwrap(),
        })
        .collect::<Vec<_>>();
        assert_eq!(
            rows.len(),
            20,
            "{timeframe} needs at least 20 synchronized candles"
        );

        let mut source = HashMap::new();
        for sample in [&rows[..10], &rows[10..]] {
            let fetched = client
                .klines(
                    &symbol,
                    timeframe,
                    Some(sample.first().unwrap().open_time),
                    Some(sample.last().unwrap().close_time),
                    20,
                )
                .await
                .unwrap();
            source.extend(fetched.into_iter().map(|candle| (candle.open_time, candle)));
        }

        let mut exact = 0;
        for stored in &rows {
            let remote = source
                .get(&stored.open_time)
                .unwrap_or_else(|| panic!("{timeframe} missing {}", stored.open_time));
            if remote.open == stored.open
                && remote.high == stored.high
                && remote.low == stored.low
                && remote.close == stored.close
            {
                exact += 1;
            } else {
                eprintln!(
                    "{timeframe} mismatch {} db=({},{},{},{}) binance=({},{},{},{})",
                    stored.open_time,
                    stored.open,
                    stored.high,
                    stored.low,
                    stored.close,
                    remote.open,
                    remote.high,
                    remote.low,
                    remote.close
                );
            }
        }
        assert_eq!(exact, rows.len(), "{timeframe} parity mismatch");
        println!("{timeframe}: {exact}/{} exact", rows.len());
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires an active Binance commodity market and public network access"]
async fn live_websocket_receives_all_required_event_families() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let websocket =
        BinanceWebSocket::new(environment_url("BINANCE_WS_BASE_URL", WEBSOCKET_DEFAULT));
    let mut stream = websocket.connect(&symbol).await.unwrap();
    let (kline, trade, session) = timeout(Duration::from_secs(180), async {
        let mut kline = false;
        let mut trade = false;
        let mut session = false;
        while !(kline && trade && session) {
            match stream.next().await {
                Some(Ok(MarketEvent::FormingCandle(_) | MarketEvent::ClosedCandle(_))) => {
                    kline = true;
                }
                Some(Ok(MarketEvent::AggregateTrade(_))) => trade = true,
                Some(Ok(MarketEvent::SessionChanged(_))) => session = true,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("live websocket failed: {error}"),
                None => panic!("live websocket ended before acceptance completed"),
            }
        }
        (kline, trade, session)
    })
    .await
    .expect("required live events were not all observed within 180 seconds");
    assert!(kline && trade && session);
}
