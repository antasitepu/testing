//! Research-only entry generators for pre-registered hypotheses, scored by
//! `edge-v1` like plan entries. Nothing here reaches a plan or the live system.
//! See `docs/superpowers/specs/2026-11-08-gold-time-of-day-and-trend-hypotheses-design.md`.

use crate::edge::RealEntry;
use analysis_domain::Direction;
use chrono::{DateTime, Datelike, Timelike, Weekday};
use chrono_tz::America::New_York;
use chrono_tz::Europe::London;
use chrono_tz::Tz;
use market_domain::{Candle, Timeframe};
use std::cmp::Ordering;

const HOUR_MS: i64 = 3_600_000;
/// Entry clock for TSMOM: every 25 hours, so the hour of day rotates.
const TSMOM_ROTATION_MS: i64 = 25 * HOUR_MS;
const TSMOM_LOOKBACK_MS: i64 = 365 * 24 * HOUR_MS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hypothesis {
    /// Short at the London AM fix, long at the COMEX day close.
    GoldDayCycle,
    /// Sign of the 12-month return.
    Tsmom12m,
}

impl Hypothesis {
    pub const ALL: [Self; 2] = [Self::GoldDayCycle, Self::Tsmom12m];

    pub fn name(self) -> &'static str {
        match self {
            Self::GoldDayCycle => "GOLD_DAY_CYCLE",
            Self::Tsmom12m => "TSMOM_12M",
        }
    }

    pub fn algorithm_version(self) -> &'static str {
        match self {
            Self::GoldDayCycle => "day-cycle-v1",
            Self::Tsmom12m => "tsmom-v1",
        }
    }

    pub fn parameter_version(self) -> &'static str {
        match self {
            Self::GoldDayCycle => "DAY_CYCLE-15m-v1",
            Self::Tsmom12m => "TSMOM_12M-1h-v1",
        }
    }

    /// The only timeframe each hypothesis is registered for.
    pub fn timeframe(self) -> Timeframe {
        match self {
            Self::GoldDayCycle => Timeframe::M15,
            Self::Tsmom12m => Timeframe::H1,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|hypothesis| hypothesis.name() == name)
    }
}

/// True when `open_time` is exactly `hour:minute` local time on a weekday.
fn opens_at(open_time: i64, zone: Tz, hour: u32, minute: u32) -> bool {
    let Some(utc) = DateTime::from_timestamp_millis(open_time) else {
        return false;
    };
    let local = utc.with_timezone(&zone);
    !matches!(local.weekday(), Weekday::Sat | Weekday::Sun)
        && local.hour() == hour
        && local.minute() == minute
        && local.second() == 0
        && local.nanosecond() == 0
}

fn entry_at(candle: &Candle, direction: Direction) -> RealEntry {
    RealEntry {
        entry_time: candle.open_time,
        entry_price: candle.open,
        direction,
    }
}

/// H1: short at the open of the 10:30 London bar, long at the open of the
/// 13:30 New York bar, Monday to Friday local date.
pub fn day_cycle_entries(candles: &[Candle]) -> Vec<RealEntry> {
    candles
        .iter()
        .filter_map(|candle| {
            if opens_at(candle.open_time, London, 10, 30) {
                Some(entry_at(candle, Direction::Sell))
            } else if opens_at(candle.open_time, New_York, 13, 30) {
                Some(entry_at(candle, Direction::Buy))
            } else {
                None
            }
        })
        .collect()
}

fn last_close_at_or_before(daily: &[Candle], time: i64) -> Option<&Candle> {
    let after = daily.partition_point(|candle| candle.close_time <= time);
    after.checked_sub(1).map(|index| &daily[index])
}

/// H2: on bars opening at a multiple of 25 hours since the epoch, enter in the
/// direction of the 12-month return of closed daily candles. A bar fewer than
/// `spacing_bars` after the previous entry is skipped.
pub fn tsmom_entries(candles: &[Candle], daily: &[Candle], spacing_bars: usize) -> Vec<RealEntry> {
    let mut entries = Vec::new();
    let mut previous: Option<usize> = None;
    for (index, candle) in candles.iter().enumerate() {
        if candle.open_time.rem_euclid(TSMOM_ROTATION_MS) != 0
            || previous.is_some_and(|last| index - last < spacing_bars)
        {
            continue;
        }
        let Some(now) = last_close_at_or_before(daily, candle.open_time) else {
            continue;
        };
        let Some(then) = last_close_at_or_before(daily, candle.open_time - TSMOM_LOOKBACK_MS)
        else {
            continue;
        };
        // Prices are positive, so the sign of ln(now / then) is the sign of now - then.
        let direction = match now.close.cmp(&then.close) {
            Ordering::Greater => Direction::Buy,
            Ordering::Less => Direction::Sell,
            Ordering::Equal => continue,
        };
        entries.push(entry_at(candle, direction));
        previous = Some(index);
    }
    entries
}
