use analysis_core::{calculate_bollinger, calculate_chandelier, next_long_stop, next_short_stop};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use std::str::FromStr;

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn candle(index: usize, high: i64, low: i64, close: i64) -> Candle {
    let open_time = i64::try_from(index).unwrap() * 60_000;
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(close, 0),
        Decimal::new(high, 0),
        Decimal::new(low, 0),
        Decimal::new(close, 0),
    )
    .unwrap()
}

#[test]
fn bollinger_uses_population_deviation_bandwidth_and_zscore() {
    let output = calculate_bollinger(
        &[Decimal::ONE, Decimal::TWO, Decimal::new(3, 0)],
        3,
        Decimal::TWO,
    )
    .unwrap();
    let fact = output[2].as_ref().unwrap();
    assert_eq!(fact.middle, Decimal::TWO);
    assert_eq!(fact.deviation, decimal("0.816497"));
    assert_eq!(fact.upper, decimal("3.632993"));
    assert_eq!(fact.lower, decimal("0.367007"));
    assert_eq!(fact.bandwidth, decimal("1.632993"));
    assert_eq!(fact.zscore, decimal("1.224745"));

    let flat = calculate_bollinger(&[Decimal::TEN; 3], 3, Decimal::TWO).unwrap();
    let flat = flat[2].as_ref().unwrap();
    assert_eq!(flat.upper, Decimal::TEN);
    assert_eq!(flat.lower, Decimal::TEN);
    assert_eq!(flat.zscore, Decimal::ZERO);
}

#[test]
fn chandelier_produces_long_short_candidates_effective_next_candle() {
    let candles = vec![
        candle(0, 10, 5, 8),
        candle(1, 12, 6, 10),
        candle(2, 11, 4, 9),
    ];
    let atr = vec![None, None, Some(Decimal::TWO)];
    let output = calculate_chandelier(&candles, &atr, 3, Decimal::new(3, 0)).unwrap();
    let fact = output[2].as_ref().unwrap();
    assert_eq!(fact.long_candidate, Decimal::new(6, 0));
    assert_eq!(fact.short_candidate, Decimal::new(10, 0));
    assert_eq!(fact.source_end_time, candles[2].close_time);
    assert_eq!(fact.effective_at, candles[2].close_time + 1);
}

#[test]
fn trailing_stops_only_move_toward_profit() {
    assert_eq!(
        next_long_stop(Decimal::new(100, 0), Decimal::new(90, 0)),
        Decimal::new(100, 0)
    );
    assert_eq!(
        next_long_stop(Decimal::new(100, 0), Decimal::new(110, 0)),
        Decimal::new(110, 0)
    );
    assert_eq!(
        next_short_stop(Decimal::new(100, 0), Decimal::new(110, 0)),
        Decimal::new(100, 0)
    );
    assert_eq!(
        next_short_stop(Decimal::new(100, 0), Decimal::new(90, 0)),
        Decimal::new(90, 0)
    );
}

#[test]
fn bollinger_reports_overflow_for_extreme_prices_instead_of_panicking() {
    let closes: Vec<_> = (0..20)
        .map(|index| {
            if index % 2 == 0 {
                Decimal::new(1, 0)
            } else {
                Decimal::from_i128_with_scale(10_i128.pow(27), 0)
            }
        })
        .collect();
    assert_eq!(
        calculate_bollinger(&closes, 20, Decimal::TWO),
        Err(analysis_core::IndicatorError::ArithmeticOverflow)
    );
}
