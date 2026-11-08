use analysis_domain::Direction;
use backtest_engine::hypotheses::{Hypothesis, day_cycle_entries, tsmom_entries};
use chrono::{TimeZone, Utc};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
        .timestamp_millis()
}

fn candle(timeframe: Timeframe, open_time: i64, duration: i64, close: i64) -> Candle {
    let price = Decimal::new(close, 2);
    Candle::new(
        Symbol::new("XAUUSD").unwrap(),
        timeframe,
        open_time,
        open_time + duration - 1,
        price,
        price,
        price,
        price,
    )
    .unwrap()
}

fn quarter_hours_of(day_start: i64) -> impl Iterator<Item = Candle> {
    (0..96).map(move |slot| {
        candle(
            Timeframe::M15,
            day_start + slot * 15 * MINUTE,
            15 * MINUTE,
            10_000,
        )
    })
}

#[test]
fn hypotheses_are_registered_for_one_timeframe_each() {
    assert_eq!(
        Hypothesis::from_name("GOLD_DAY_CYCLE"),
        Some(Hypothesis::GoldDayCycle)
    );
    assert_eq!(
        Hypothesis::from_name("TSMOM_12M"),
        Some(Hypothesis::Tsmom12m)
    );
    assert_eq!(Hypothesis::from_name("ICT"), None);
    assert_eq!(Hypothesis::GoldDayCycle.timeframe(), Timeframe::M15);
    assert_eq!(Hypothesis::Tsmom12m.timeframe(), Timeframe::H1);
}

#[test]
fn day_cycle_follows_london_and_new_york_clocks_across_daylight_saving() {
    // Winter; US summer only; both summer; UK winter while US is still summer; a Saturday.
    let days = [
        utc(2021, 3, 1, 0, 0),
        utc(2021, 3, 15, 0, 0),
        utc(2021, 4, 5, 0, 0),
        utc(2021, 11, 1, 0, 0),
        utc(2021, 3, 6, 0, 0),
    ];
    let candles: Vec<Candle> = days.into_iter().flat_map(quarter_hours_of).collect();
    let entries: Vec<(i64, Direction)> = day_cycle_entries(&candles)
        .into_iter()
        .map(|entry| (entry.entry_time, entry.direction))
        .collect();
    assert_eq!(
        entries,
        vec![
            (utc(2021, 3, 1, 10, 30), Direction::Sell),
            (utc(2021, 3, 1, 18, 30), Direction::Buy),
            (utc(2021, 3, 15, 10, 30), Direction::Sell),
            (utc(2021, 3, 15, 17, 30), Direction::Buy),
            (utc(2021, 4, 5, 9, 30), Direction::Sell),
            (utc(2021, 4, 5, 17, 30), Direction::Buy),
            (utc(2021, 11, 1, 10, 30), Direction::Sell),
            (utc(2021, 11, 1, 17, 30), Direction::Buy),
        ]
    );
}

#[test]
fn day_cycle_enters_at_the_bar_open_and_skips_missing_bars() {
    let day = utc(2021, 3, 1, 0, 0);
    let candles: Vec<Candle> = quarter_hours_of(day)
        .filter(|candle| candle.open_time != utc(2021, 3, 1, 18, 30))
        .collect();
    let entries = day_cycle_entries(&candles);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].entry_price, Decimal::new(10_000, 2));
}

/// Daily candles from 2020-01-01; `close_of(index)` in cents.
fn daily(days: i64, close_of: impl Fn(i64) -> i64) -> Vec<Candle> {
    let start = utc(2020, 1, 1, 0, 0);
    (0..days)
        .map(|index| candle(Timeframe::D1, start + index * DAY, DAY, close_of(index)))
        .collect()
}

fn hours(from: i64, count: i64) -> Vec<Candle> {
    (0..count)
        .map(|index| candle(Timeframe::H1, from + index * HOUR, HOUR, 10_000))
        .collect()
}

#[test]
fn tsmom_enters_every_25_hours_in_the_direction_of_the_12_month_return() {
    let rising = daily(420, |index| 10_000 + index);
    let candles = hours(utc(2021, 1, 20, 0, 0), 24 * 10);
    let entries = tsmom_entries(&candles, &rising, 24);
    let expected: Vec<i64> = candles
        .iter()
        .map(|candle| candle.open_time)
        .filter(|time| time.rem_euclid(25 * HOUR) == 0)
        .collect();
    assert_eq!(expected.len(), 9);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.entry_time)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.direction == Direction::Buy)
    );

    let falling = daily(420, |index| 20_000 - index);
    assert!(
        tsmom_entries(&candles, &falling, 24)
            .iter()
            .all(|entry| entry.direction == Direction::Sell)
    );
    let flat = daily(420, |_| 10_000);
    assert!(tsmom_entries(&candles, &flat, 24).is_empty());
}

#[test]
fn tsmom_needs_a_year_of_daily_closes_before_the_entry() {
    let rising = daily(420, |index| 10_000 + index);
    // Less than 365 days after the first daily close.
    let early = hours(utc(2020, 12, 1, 0, 0), 24 * 5);
    assert!(tsmom_entries(&early, &rising, 24).is_empty());
}

#[test]
fn tsmom_skips_an_entry_fewer_than_24_bars_after_the_previous_one() {
    let rising = daily(420, |index| 10_000 + index);
    let first = (utc(2021, 1, 20, 0, 0) / (25 * HOUR) + 1) * 25 * HOUR;
    // Six bars from `first`, a gap, then 60 bars from the next rotation time on:
    // `first + 25h` is only 6 bars later and is skipped; 50h and 75h are kept.
    let mut candles = hours(first, 6);
    candles.extend(hours(first + 25 * HOUR, 60));
    let entries: Vec<i64> = tsmom_entries(&candles, &rising, 24)
        .into_iter()
        .map(|entry| entry.entry_time)
        .collect();
    assert_eq!(entries, vec![first, first + 50 * HOUR, first + 75 * HOUR]);
}
