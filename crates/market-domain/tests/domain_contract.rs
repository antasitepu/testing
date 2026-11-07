use market_domain::{
    AggregateTrade, AggregateTradeError, Candle, CandleError, Market, MarketError, MarketSession,
    MarketStatus, SessionStatus, Symbol, SymbolError, Timeframe,
};
use rust_decimal::Decimal;
use std::str::FromStr;

#[test]
fn all_supported_timeframes_round_trip() {
    for raw in ["1m", "5m", "15m", "30m", "1h", "4h", "1d"] {
        let timeframe = Timeframe::from_str(raw).unwrap();
        assert_eq!(timeframe.as_str(), raw);
    }
}

#[test]
fn timeframe_durations_match_binance_boundaries() {
    assert_eq!(Timeframe::M1.duration_ms(), 60_000);
    assert_eq!(Timeframe::M5.duration_ms(), 300_000);
    assert_eq!(Timeframe::M15.duration_ms(), 900_000);
    assert_eq!(Timeframe::M30.duration_ms(), 1_800_000);
    assert_eq!(Timeframe::H1.duration_ms(), 3_600_000);
    assert_eq!(Timeframe::H4.duration_ms(), 14_400_000);
    assert_eq!(Timeframe::D1.duration_ms(), 86_400_000);
}

#[test]
fn symbol_requires_uppercase_ascii_letters_or_digits() {
    assert_eq!(Symbol::new(""), Err(SymbolError::Empty));
    assert_eq!(Symbol::new("xauusdt"), Err(SymbolError::InvalidCharacter));
    assert_eq!(Symbol::new("XAU-USDT"), Err(SymbolError::InvalidCharacter));
    assert_eq!(Symbol::new("XAUUSDT").unwrap().as_str(), "XAUUSDT");
}

#[test]
fn candle_rejects_high_below_body() {
    let result = Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M1,
        1_000,
        60_999,
        Decimal::new(410000, 2),
        Decimal::new(409900, 2),
        Decimal::new(409800, 2),
        Decimal::new(409950, 2),
    );
    assert_eq!(result, Err(CandleError::HighBelowBody));
}

#[test]
fn candle_rejects_invalid_time_low_and_non_positive_prices() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let valid = (
        Decimal::new(410000, 2),
        Decimal::new(410100, 2),
        Decimal::new(409900, 2),
        Decimal::new(410050, 2),
    );

    assert_eq!(
        Candle::new(
            symbol.clone(),
            Timeframe::M1,
            60_999,
            1_000,
            valid.0,
            valid.1,
            valid.2,
            valid.3,
        ),
        Err(CandleError::InvalidTimeRange)
    );
    assert_eq!(
        Candle::new(
            symbol.clone(),
            Timeframe::M1,
            1_000,
            60_999,
            valid.0,
            valid.1,
            Decimal::new(410060, 2),
            valid.3,
        ),
        Err(CandleError::LowAboveBody)
    );
    assert_eq!(
        Candle::new(
            symbol,
            Timeframe::M1,
            1_000,
            60_999,
            Decimal::ZERO,
            valid.1,
            valid.2,
            valid.3,
        ),
        Err(CandleError::NonPositivePrice)
    );
}

#[test]
fn no_trading_session_contains_boundary_time() {
    let session = MarketSession::new("COMMODITY", SessionStatus::NoTrading, 100, 200, 150).unwrap();
    assert!(session.contains(100));
    assert!(!session.contains(200));
}

#[test]
fn session_rejects_empty_market_and_invalid_range() {
    assert!(MarketSession::new("", SessionStatus::Regular, 100, 200, 150).is_err());
    assert!(MarketSession::new("COMMODITY", SessionStatus::Regular, 200, 200, 150).is_err());
}

#[test]
fn market_requires_positive_tick_and_non_negative_onboard_time() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    assert_eq!(
        Market::new(
            symbol.clone(),
            true,
            MarketStatus::Trading,
            Decimal::ZERO,
            1_000,
            "COMMODITY",
        ),
        Err(MarketError::NonPositiveTick)
    );
    assert_eq!(
        Market::new(
            symbol,
            true,
            MarketStatus::Trading,
            Decimal::new(1, 2),
            -1,
            "COMMODITY",
        ),
        Err(MarketError::NegativeOnboardTime)
    );
}

#[test]
fn aggregate_trade_requires_consistent_identity_and_positive_price() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    assert_eq!(
        AggregateTrade::new(
            symbol.clone(),
            10,
            12,
            11,
            1_000,
            1_000,
            Decimal::new(410000, 2),
        ),
        Err(AggregateTradeError::InvalidTradeIdRange)
    );
    assert_eq!(
        AggregateTrade::new(symbol, 10, 10, 10, 1_000, 1_000, Decimal::ZERO),
        Err(AggregateTradeError::NonPositivePrice)
    );
}
