use chrono::DateTime;
use market_domain::{Candle, MarketSession, SessionStatus, Symbol, Timeframe};
use market_service::{
    CandleSessionClass, CommoditySessionGenerator, SessionRecord, SessionSource,
    TradableCandlePolicy, canonicalize_sessions,
};
use rust_decimal::Decimal;

#[test]
fn classifies_full_coverage_and_partial_higher_timeframe_ranges() {
    let sessions = vec![
        session(SessionStatus::Regular, 0, 60_000),
        session(SessionStatus::NoTrading, 60_000, 120_000),
        session(SessionStatus::Regular, 120_000, 240_000),
    ];
    let policy = TradableCandlePolicy;

    assert_eq!(
        policy.classify(&candle(Timeframe::M1, 0, 59_999), &sessions),
        CandleSessionClass::Tradable
    );
    assert_eq!(
        policy.classify(&candle(Timeframe::M1, 60_000, 119_999), &sessions),
        CandleSessionClass::NoTrading
    );
    assert_eq!(
        policy.classify(&candle(Timeframe::H1, 30_000, 149_999), &sessions),
        CandleSessionClass::Tradable
    );
    assert_eq!(
        policy.classify(&candle(Timeframe::M1, 240_000, 299_999), &sessions),
        CandleSessionClass::Unknown
    );
}

#[test]
fn first_tradable_open_skips_closed_bars_and_stops_where_coverage_ends() {
    let sessions = vec![
        session(SessionStatus::Regular, 0, 60_000),
        session(SessionStatus::NoTrading, 60_000, 210_000),
        session(SessionStatus::Regular, 210_000, 300_000),
    ];
    let policy = TradableCandlePolicy;

    assert_eq!(
        policy.first_tradable_open(Timeframe::M1, 0, &sessions),
        Some(0)
    );
    // 180_000 opens inside the closure but reaches the regular session, so it trades.
    assert_eq!(
        policy.first_tradable_open(Timeframe::M1, 60_000, &sessions),
        Some(180_000)
    );
    assert_eq!(
        policy.first_tradable_open(Timeframe::M1, 300_000, &sessions),
        None
    );
    assert_eq!(
        policy.first_tradable_open(Timeframe::M1, 60_000, &sessions[..2]),
        None
    );
}

#[test]
fn weekdays_trade_without_a_daily_break() {
    for (start, end) in [
        ("2026-01-05T00:00:00Z", "2026-01-09T00:00:00Z"),
        ("2026-07-06T00:00:00Z", "2026-07-10T00:00:00Z"),
    ] {
        let records = CommoditySessionGenerator
            .generate(utc(start), utc(end), 1)
            .unwrap();

        assert_eq!(records.len(), 1, "{start}");
        assert_eq!(records[0].session.status, SessionStatus::Regular);
        assert_eq!(records[0].session.start_time, utc(start));
        assert_eq!(records[0].session.end_time, utc(end));
    }
}

#[test]
fn weekend_closure_spans_friday_close_to_sunday_open_in_new_york_time() {
    let generator = CommoditySessionGenerator;
    let winter = generator
        .generate(utc("2026-01-09T00:00:00Z"), utc("2026-01-12T00:00:00Z"), 1)
        .unwrap();
    let summer = generator
        .generate(utc("2026-07-10T00:00:00Z"), utc("2026-07-13T00:00:00Z"), 1)
        .unwrap();

    assert!(has_no_trading(
        &winter,
        utc("2026-01-09T22:00:00Z"),
        utc("2026-01-11T23:00:00Z")
    ));
    assert!(has_no_trading(
        &summer,
        utc("2026-07-10T21:00:00Z"),
        utc("2026-07-12T22:00:00Z")
    ));
}

#[test]
fn generated_sessions_match_the_binance_commodity_schedule() {
    // /fapi/v1/tradingSchedule on 2026-11-06, for a span that crosses the end of daylight saving.
    let binance = [
        (
            SessionStatus::Regular,
            "2026-10-29T04:00:00Z",
            "2026-10-30T21:00:00Z",
        ),
        (
            SessionStatus::NoTrading,
            "2026-10-30T21:00:00Z",
            "2026-11-01T23:00:00Z",
        ),
        (
            SessionStatus::Regular,
            "2026-11-01T23:00:00Z",
            "2026-11-06T22:00:00Z",
        ),
        (
            SessionStatus::NoTrading,
            "2026-11-06T22:00:00Z",
            "2026-11-08T23:00:00Z",
        ),
        (
            SessionStatus::Regular,
            "2026-11-08T23:00:00Z",
            "2026-11-13T22:00:00Z",
        ),
    ];

    let generated = CommoditySessionGenerator
        .generate(utc("2026-10-29T04:00:00Z"), utc("2026-11-13T22:00:00Z"), 1)
        .unwrap();

    let generated: Vec<_> = generated
        .iter()
        .map(|record| {
            (
                record.session.status,
                record.session.start_time,
                record.session.end_time,
            )
        })
        .collect();
    let expected: Vec<_> = binance
        .iter()
        .map(|(status, start, end)| (*status, utc(start), utc(end)))
        .collect();
    assert_eq!(generated, expected);
}

#[test]
fn canonicalization_splits_baseline_with_higher_precedence_override() {
    let baseline = record(
        SessionStatus::Regular,
        0,
        180_000,
        SessionSource::DerivedWeekly,
        None,
    );
    let override_record = record(
        SessionStatus::NoTrading,
        60_000,
        120_000,
        SessionSource::ManualOverride,
        Some("holiday-closure"),
    );

    let canonical = canonicalize_sessions(0, 180_000, &[baseline, override_record]).unwrap();

    assert_eq!(canonical.len(), 3);
    assert_eq!(canonical[0].session.start_time, 0);
    assert_eq!(canonical[0].session.end_time, 60_000);
    assert_eq!(canonical[1].session.status, SessionStatus::NoTrading);
    assert_eq!(canonical[1].source, SessionSource::ManualOverride);
    assert_eq!(canonical[2].session.start_time, 120_000);
    assert_eq!(canonical[2].session.end_time, 180_000);
}

#[test]
fn canonicalization_merges_semantically_equal_neighbors_and_keeps_latest_event() {
    let mut first = record(
        SessionStatus::Regular,
        0,
        60_000,
        SessionSource::BinanceApi,
        None,
    );
    first.session.source_event_time = 10;
    let mut second = record(
        SessionStatus::Regular,
        60_000,
        120_000,
        SessionSource::BinanceApi,
        None,
    );
    second.session.source_event_time = 20;

    let canonical = canonicalize_sessions(0, 120_000, &[first, second]).unwrap();

    assert_eq!(canonical.len(), 1);
    assert_eq!(canonical[0].session.start_time, 0);
    assert_eq!(canonical[0].session.end_time, 120_000);
    assert_eq!(canonical[0].session.source_event_time, 20);
}

fn candle(timeframe: Timeframe, open_time: i64, close_time: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        timeframe,
        open_time,
        close_time,
        Decimal::new(4_100, 0),
        Decimal::new(4_101, 0),
        Decimal::new(4_099, 0),
        Decimal::new(4_100, 0),
    )
    .unwrap()
}

fn session(status: SessionStatus, start_time: i64, end_time: i64) -> MarketSession {
    MarketSession::new("COMMODITY", status, start_time, end_time, end_time).unwrap()
}

fn record(
    status: SessionStatus,
    start_time: i64,
    end_time: i64,
    source: SessionSource,
    source_reference: Option<&str>,
) -> SessionRecord {
    SessionRecord {
        session: session(status, start_time, end_time),
        source,
        source_reference: source_reference.map(str::to_owned),
    }
}

fn has_no_trading(records: &[SessionRecord], start_time: i64, end_time: i64) -> bool {
    records.iter().any(|record| {
        record.session.status == SessionStatus::NoTrading
            && record.session.start_time == start_time
            && record.session.end_time == end_time
    })
}

fn utc(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .timestamp_millis()
}
