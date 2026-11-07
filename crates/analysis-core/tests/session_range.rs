use analysis_core::{SessionRangeParameters, detect_session_ranges};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;

/// 2026-03-02 00:00 UTC, a Monday.
const MONDAY: i64 = 1_772_409_600_000;
/// 2026-03-01 00:00 UTC, a Sunday.
const SUNDAY: i64 = 1_772_323_200_000;
const HOUR: i64 = 3_600_000;

fn candle(timeframe: Timeframe, open_time: i64, high: i64, low: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        timeframe,
        open_time,
        open_time + timeframe.duration_ms() - 1,
        Decimal::new(low, 0),
        Decimal::new(high, 0),
        Decimal::new(low, 0),
        Decimal::new(high, 0),
    )
    .unwrap()
}

/// Bars from `start` (inclusive) to `end` (exclusive), each 10 wide, with a
/// different high per bar so the range extremes are identifiable.
fn bars(timeframe: Timeframe, start: i64, end: i64) -> Vec<Candle> {
    let duration = timeframe.duration_ms();
    (0..)
        .map(|index| start + index * duration)
        .take_while(|open_time| *open_time < end)
        .map(|open_time| {
            let step = (open_time - start) / duration;
            candle(timeframe, open_time, 4_100 + step % 7, 4_090 - step % 5)
        })
        .collect()
}

fn atr(candles: &[Candle], value: i64) -> Vec<Option<Decimal>> {
    vec![Some(Decimal::new(value, 0)); candles.len()]
}

#[test]
fn complete_five_minute_window_gives_one_fact_at_the_last_range_bar() {
    let candles = bars(Timeframe::M5, MONDAY, MONDAY + 9 * HOUR);
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();

    assert_eq!(facts.len(), 1);
    let (index, fact) = &facts[0];
    // 06:55 is the last bar inside [00:00, 07:00); 07:00 is outside.
    assert_eq!(candles[*index].open_time, MONDAY + 7 * HOUR - 300_000);
    assert_eq!(fact.session_day, MONDAY / 86_400_000);
    assert_eq!(fact.window_start, MONDAY);
    assert_eq!(fact.window_end, MONDAY + 7 * HOUR);
    assert_eq!(fact.bar_count, 84);
    assert_eq!(fact.expected_bar_count, 84);
    assert!(fact.is_complete());
    let inside = &candles[..84];
    assert_eq!(fact.high, inside.iter().map(|c| c.high).max().unwrap());
    assert_eq!(fact.low, inside.iter().map(|c| c.low).min().unwrap());
    assert_eq!(fact.atr_at_range, Some(Decimal::TWO));
}

#[test]
fn bars_after_the_window_do_not_change_the_range() {
    let mut candles = bars(Timeframe::M5, MONDAY, MONDAY + 7 * HOUR);
    let baseline =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();
    candles.push(candle(Timeframe::M5, MONDAY + 7 * HOUR, 9_999, 1));
    let extended =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();
    assert_eq!(baseline, extended);
}

#[test]
fn one_hour_range_has_seven_bars_and_a_normalized_width() {
    let candles = bars(Timeframe::H1, MONDAY, MONDAY + 12 * HOUR);
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 3), SessionRangeParameters::ASIA).unwrap();

    assert_eq!(facts.len(), 1);
    let (index, fact) = &facts[0];
    assert_eq!(candles[*index].open_time, MONDAY + 6 * HOUR);
    assert_eq!(fact.bar_count, 7);
    assert_eq!(fact.expected_bar_count, 7);
    // width / (atr * sqrt(7)) with width = high - low of the first seven bars.
    let width = fact.high - fact.low;
    let expected = (width / (Decimal::new(3, 0) * Decimal::new(2_645_751_311_065, 12))).round_dp(6);
    assert_eq!(fact.normalized_width, Some(expected));
}

#[test]
fn sunday_window_inside_the_weekend_closure_has_no_fact() {
    // Session-clean history: Friday bars, then the Sunday reopen at 23:00 UTC.
    let mut candles = bars(
        Timeframe::M15,
        SUNDAY - 2 * 86_400_000,
        SUNDAY - 2 * 86_400_000 + 21 * HOUR,
    );
    candles.extend(bars(Timeframe::M15, SUNDAY + 23 * HOUR, MONDAY + 8 * HOUR));
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();

    let days: Vec<_> = facts.iter().map(|(_, fact)| fact.session_day).collect();
    assert!(!days.contains(&(SUNDAY / 86_400_000)));
    // Monday's range holds only Monday's [00:00, 07:00) bars, not the Sunday reopen bars.
    let monday = facts
        .iter()
        .find(|(_, fact)| fact.session_day == MONDAY / 86_400_000)
        .map(|(_, fact)| fact)
        .unwrap();
    assert_eq!(monday.bar_count, 28);
    assert!(monday.is_complete());
}

#[test]
fn missing_window_bars_mark_the_range_incomplete_or_drop_it() {
    let mut candles = bars(Timeframe::M30, MONDAY, MONDAY + 8 * HOUR);
    candles.remove(3);
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].1.bar_count, 13);
    assert_eq!(facts[0].1.expected_bar_count, 14);
    assert!(!facts[0].1.is_complete());

    // Without the 06:30 bar the range is never confirmed.
    let mut candles = bars(Timeframe::M30, MONDAY, MONDAY + 8 * HOUR);
    candles.retain(|candle| candle.open_time != MONDAY + 6 * HOUR + 30 * 60_000);
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();
    assert!(facts.is_empty());
}

#[test]
fn missing_atr_leaves_width_unknown() {
    let candles = bars(Timeframe::H1, MONDAY, MONDAY + 8 * HOUR);
    let facts = detect_session_ranges(
        &candles,
        &vec![None; candles.len()],
        SessionRangeParameters::ASIA,
    )
    .unwrap();
    assert_eq!(facts[0].1.atr_at_range, None);
    assert_eq!(facts[0].1.normalized_width, None);
}

#[test]
fn timeframes_that_do_not_divide_the_window_give_no_facts() {
    let candles = bars(Timeframe::H4, MONDAY, MONDAY + 24 * HOUR);
    let facts =
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).unwrap();
    assert!(facts.is_empty());
}

#[test]
fn unordered_candles_or_mismatched_atr_are_rejected() {
    let mut candles = bars(Timeframe::M5, MONDAY, MONDAY + HOUR);
    assert!(detect_session_ranges(&candles, &[], SessionRangeParameters::ASIA).is_err());
    candles.swap(0, 1);
    assert!(
        detect_session_ranges(&candles, &atr(&candles, 2), SessionRangeParameters::ASIA).is_err()
    );
}
