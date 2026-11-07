use gold_monitoring_server::runtime_analysis_candles::filter_runtime_candles;
use market_domain::{Candle, MarketSession, SessionStatus, Symbol, Timeframe};
use rust_decimal::Decimal;

#[test]
fn runtime_filter_matches_session_clean_sequence_and_fails_on_unknown() {
    let raw = vec![candle(0), candle(60_000), candle(120_000)];
    let complete = vec![
        session(SessionStatus::Regular, 0, 60_000),
        session(SessionStatus::NoTrading, 60_000, 120_000),
        session(SessionStatus::Regular, 120_000, 180_000),
    ];

    let filtered = filter_runtime_candles(raw.clone(), &complete, 0).unwrap();

    assert_eq!(
        filtered
            .iter()
            .map(|candle| candle.open_time)
            .collect::<Vec<_>>(),
        vec![0, 120_000]
    );
    assert!(filter_runtime_candles(raw, &complete[..2], 0).is_err());
}

#[test]
fn candle_opening_before_listing_is_excluded_instead_of_failing() {
    let onboard_time = 60_000;
    let raw = vec![candle(0), candle(60_000)];
    let sessions = vec![session(SessionStatus::Regular, 60_000, 120_000)];

    let filtered = filter_runtime_candles(raw.clone(), &sessions, onboard_time).unwrap();

    assert_eq!(
        filtered
            .iter()
            .map(|candle| candle.open_time)
            .collect::<Vec<_>>(),
        vec![60_000]
    );
    assert!(filter_runtime_candles(raw, &sessions, 0).is_err());
}

fn candle(open_time: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(4_100, 0),
        Decimal::new(4_101, 0),
        Decimal::new(4_099, 0),
        Decimal::new(4_100, 0),
    )
    .unwrap()
}

fn session(status: SessionStatus, start_time: i64, end_time: i64) -> MarketSession {
    MarketSession::new("COMMODITY", status, start_time, end_time, 1).unwrap()
}
