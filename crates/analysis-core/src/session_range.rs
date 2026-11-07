//! Session opening range: the high and low of a fixed UTC window on each day.
//!
//! One fact per UTC day whose last window bar is present, confirmed at that bar's
//! close. Days missing earlier window bars still produce a fact, with
//! `bar_count < expected_bar_count`, so consumers can refuse incomplete ranges.

use crate::IndicatorError;
use market_domain::Candle;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DAY_MS: i64 = 86_400_000;
const MINUTE_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRangeParameters {
    /// Window start, minutes after 00:00 UTC (inclusive).
    pub start_minute_utc: i64,
    /// Window end, minutes after 00:00 UTC (exclusive).
    pub end_minute_utc: i64,
}

impl SessionRangeParameters {
    /// The Asia session window used by `session-range-runtime-v1`: [00:00, 07:00) UTC.
    pub const ASIA: Self = Self {
        start_minute_utc: 0,
        end_minute_utc: 420,
    };
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRangeFact {
    /// UTC day index: `open_time.div_euclid(86_400_000)`.
    pub session_day: i64,
    pub window_start: i64,
    pub window_end: i64,
    pub high: Decimal,
    pub low: Decimal,
    pub bar_count: usize,
    pub expected_bar_count: usize,
    pub atr_at_range: Option<Decimal>,
    pub normalized_width: Option<Decimal>,
}

impl SessionRangeFact {
    pub fn is_complete(&self) -> bool {
        self.bar_count == self.expected_bar_count
    }
}

struct DayAccumulator {
    high: Decimal,
    low: Decimal,
    bar_count: usize,
    last_index: Option<usize>,
}

/// Returns `(index of the last window bar, fact)` pairs in candle order.
///
/// `atr` must hold one value per candle. Timeframes whose duration does not divide
/// the window yield no facts.
pub fn detect_session_ranges(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    parameters: SessionRangeParameters,
) -> Result<Vec<(usize, SessionRangeFact)>, IndicatorError> {
    if candles.windows(2).any(|window| {
        window[0].open_time >= window[1].open_time
            || window[0].symbol != window[1].symbol
            || window[0].timeframe != window[1].timeframe
    }) || atr.len() != candles.len()
    {
        return Err(IndicatorError::InvalidCandleSequence);
    }
    if parameters.start_minute_utc < 0
        || parameters.start_minute_utc >= parameters.end_minute_utc
        || parameters.end_minute_utc > 24 * 60
    {
        return Err(IndicatorError::InvalidPeriod);
    }
    let Some(first) = candles.first() else {
        return Ok(Vec::new());
    };
    let duration = first.timeframe.duration_ms();
    let window_ms = (parameters.end_minute_utc - parameters.start_minute_utc) * MINUTE_MS;
    if duration <= 0 || window_ms % duration != 0 || duration > window_ms {
        return Ok(Vec::new());
    }
    let expected_bar_count =
        usize::try_from(window_ms / duration).map_err(|_| IndicatorError::ArithmeticOverflow)?;
    let start_offset = parameters.start_minute_utc * MINUTE_MS;
    let end_offset = parameters.end_minute_utc * MINUTE_MS;

    let mut days: BTreeMap<i64, DayAccumulator> = BTreeMap::new();
    for (index, candle) in candles.iter().enumerate() {
        let day = candle.open_time.div_euclid(DAY_MS);
        let offset = candle.open_time - day * DAY_MS;
        if offset < start_offset || offset >= end_offset {
            continue;
        }
        let entry = days.entry(day).or_insert(DayAccumulator {
            high: candle.high,
            low: candle.low,
            bar_count: 0,
            last_index: None,
        });
        entry.high = entry.high.max(candle.high);
        entry.low = entry.low.min(candle.low);
        entry.bar_count += 1;
        if offset == end_offset - duration {
            entry.last_index = Some(index);
        }
    }

    let mut output = Vec::new();
    for (day, accumulator) in days {
        let Some(last_index) = accumulator.last_index else {
            continue;
        };
        let atr_at_range = atr[last_index];
        let normalized_width = atr_at_range.and_then(|atr_value| {
            let bars = Decimal::from(u64::try_from(accumulator.bar_count).ok()?);
            let scale = atr_value.checked_mul(decimal_sqrt(bars)?)?;
            if scale <= Decimal::ZERO {
                return None;
            }
            Some(((accumulator.high - accumulator.low) / scale).round_dp(6))
        });
        output.push((
            last_index,
            SessionRangeFact {
                session_day: day,
                window_start: day * DAY_MS + start_offset,
                window_end: day * DAY_MS + end_offset,
                high: accumulator.high,
                low: accumulator.low,
                bar_count: accumulator.bar_count,
                expected_bar_count,
                atr_at_range,
                normalized_width,
            },
        ));
    }
    output.sort_by_key(|(index, _)| *index);
    Ok(output)
}

/// Square root by Newton's method in decimal arithmetic, so results do not depend
/// on floating point. Returns `None` for non-positive input.
fn decimal_sqrt(value: Decimal) -> Option<Decimal> {
    if value <= Decimal::ZERO {
        return None;
    }
    let mut estimate = value.max(Decimal::ONE);
    for _ in 0..100 {
        let next = (estimate + value.checked_div(estimate)?) / Decimal::TWO;
        if next == estimate {
            break;
        }
        estimate = next;
    }
    Some(estimate.round_dp(12))
}

#[cfg(test)]
mod tests {
    use super::decimal_sqrt;
    use rust_decimal::Decimal;

    #[test]
    fn decimal_sqrt_matches_known_roots() {
        assert_eq!(decimal_sqrt(Decimal::from(49)), Some(Decimal::from(7)));
        assert_eq!(
            decimal_sqrt(Decimal::from(7)).map(|root| root.round_dp(6)),
            Some(Decimal::new(2_645_751, 6))
        );
        assert_eq!(decimal_sqrt(Decimal::ZERO), None);
    }
}
