use market_domain::{
    AggregateTrade, Candle, LiveStatus, MarketEvent, MarketSession, SessionStatus, Symbol,
    Timeframe,
};
use market_service::MarketHub;
use rust_decimal::Decimal;
use tokio::time::{Duration, timeout};

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn candle(timeframe: Timeframe, open_time: i64) -> Candle {
    Candle::new(
        symbol(),
        timeframe,
        open_time,
        open_time + timeframe.duration_ms() - 1,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        Decimal::new(410050, 2),
    )
    .unwrap()
}

fn trade(id: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol(),
        id,
        id,
        id,
        id * 100,
        id * 100,
        Decimal::new(410000 + id, 2),
    )
    .unwrap()
}

#[tokio::test]
async fn forming_state_is_isolated_by_symbol_and_timeframe_without_subscribers() {
    let hub = MarketHub::new(16);
    let unused_subscriber = hub.subscribe();
    drop(unused_subscriber);

    hub.apply_forming(candle(Timeframe::M1, 60_000)).await;
    hub.apply_forming(candle(Timeframe::M5, 300_000)).await;

    let m1 = hub.snapshot(&symbol(), Timeframe::M1).await;
    let m5 = hub.snapshot(&symbol(), Timeframe::M5).await;
    assert_eq!(m1.forming.unwrap().open_time, 60_000);
    assert_eq!(m5.forming.unwrap().open_time, 300_000);
}

#[tokio::test]
async fn sequential_trades_are_accepted_and_duplicates_are_ignored() {
    let hub = MarketHub::new(16);
    let mut subscriber = hub.subscribe();
    hub.apply_trade(trade(10)).await;
    hub.apply_trade(trade(10)).await;
    hub.apply_trade(trade(11)).await;

    let snapshot = hub.snapshot(&symbol(), Timeframe::M1).await;
    assert_eq!(snapshot.latest_trade.unwrap().aggregate_trade_id, 11);
    assert!(matches!(
        subscriber.recv().await.unwrap(),
        MarketEvent::AggregateTrade(item) if item.aggregate_trade_id == 10
    ));
    assert!(matches!(
        subscriber.recv().await.unwrap(),
        MarketEvent::AggregateTrade(item) if item.aggregate_trade_id == 11
    ));
    assert!(
        timeout(Duration::from_millis(10), subscriber.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn trade_sequence_gap_degrades_health_and_requests_resync() {
    let hub = MarketHub::new(16);
    let mut subscriber = hub.subscribe();
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    hub.apply_trade(trade(10)).await;
    hub.apply_trade(trade(12)).await;

    let snapshot = hub.snapshot(&symbol(), Timeframe::M1).await;
    assert_eq!(snapshot.status, LiveStatus::Degraded);
    assert_eq!(snapshot.latest_trade.unwrap().aggregate_trade_id, 12);

    let mut saw_resync = false;
    let mut saw_gap_trade = false;
    for _ in 0..5 {
        match subscriber.recv().await.unwrap() {
            MarketEvent::ResyncRequired { .. } => saw_resync = true,
            MarketEvent::AggregateTrade(item) if item.aggregate_trade_id == 12 => {
                saw_gap_trade = true;
            }
            _ => {}
        }
        if saw_resync && saw_gap_trade {
            break;
        }
    }
    assert!(saw_resync);
    assert!(saw_gap_trade);
}

#[tokio::test]
async fn no_trading_session_sets_market_closed() {
    let hub = MarketHub::new(16);
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    let session = MarketSession::new("COMMODITY", SessionStatus::NoTrading, 100, 200, 150).unwrap();

    hub.apply_session(&symbol(), session).await;

    assert_eq!(
        hub.snapshot(&symbol(), Timeframe::M1).await.status,
        LiveStatus::MarketClosed
    );
}
