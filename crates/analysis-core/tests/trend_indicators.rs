use analysis_core::{calculate_adx, calculate_ema, calculate_previous_donchian};
use analysis_domain::{Readiness, ReadinessReason};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;

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
fn ema_uses_sma_seed_and_exposes_closed_candle_slope() {
    let closes = [
        Decimal::ONE,
        Decimal::TWO,
        Decimal::new(3, 0),
        Decimal::new(4, 0),
        Decimal::new(5, 0),
    ];
    let output = calculate_ema(&closes, 3, 1).unwrap();
    assert_eq!(output.readiness, Readiness::Ready);
    assert_eq!(output.values[2], Some(Decimal::TWO));
    assert_eq!(output.values[3], Some(Decimal::new(3, 0)));
    assert_eq!(output.slopes[3], Some(Decimal::ONE));

    let warmup = calculate_ema(&closes[..2], 3, 1).unwrap();
    assert_eq!(
        warmup.readiness,
        Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory
        }
    );
}

#[test]
fn wilder_adx_reaches_100_in_a_clean_monotonic_trend() {
    let candles: Vec<_> = (0..7)
        .map(|index| {
            candle(
                index,
                10 + i64::try_from(index).unwrap(),
                8 + i64::try_from(index).unwrap(),
                9 + i64::try_from(index).unwrap(),
            )
        })
        .collect();
    let output = calculate_adx(&candles, 3).unwrap();
    assert_eq!(output.values[4], Some(Decimal::ONE_HUNDRED));
    assert_eq!(output.values[6], Some(Decimal::ONE_HUNDRED));
}

#[test]
fn donchian_excludes_the_evaluation_candle() {
    let candles = vec![
        candle(0, 10, 5, 8),
        candle(1, 12, 6, 10),
        candle(2, 11, 4, 9),
        candle(3, 100, 3, 99),
    ];
    let channels = calculate_previous_donchian(&candles, 3).unwrap();
    let breakout_channel = channels[3].as_ref().unwrap();
    assert_eq!(breakout_channel.upper, Decimal::new(12, 0));
    assert_eq!(breakout_channel.lower, Decimal::new(4, 0));
    assert_eq!(breakout_channel.source_end_time, candles[2].close_time);
    assert!(candles[3].close > breakout_channel.upper);
}
