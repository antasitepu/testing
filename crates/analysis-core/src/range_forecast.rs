//! Range forecast formulas of `vol-v1`, shared by the research benchmark
//! (`backtest_engine::volatility`) and the app's display-only forecast. A
//! `RangeModel` is never used by plans, signals or execution. See
//! `docs/superpowers/specs/2026-11-08-gold-range-forecast-benchmark-design.md`.

use chrono::{DateTime, Timelike};
use chrono_tz::America::New_York;
use market_domain::{Candle, Timeframe};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

pub const VOL_VERSION: &str = "vol-v1";
/// Forecast horizon in bars, the `edge-v1` time stop.
pub const HORIZON_BARS: usize = 24;
/// The only model a `RangeModel` may hold: it passed every test step.
pub const DISPLAY_MODEL: &str = "HAR_RANGE_SEASONAL";
const WEEK_DAYS: usize = 5;
const MONTH_DAYS: usize = 22;
const DAY_MS: i64 = 86_400_000;

fn positive_ln(value: f64) -> Option<f64> {
    (value > 0.0 && value.is_finite()).then(|| value.ln())
}

fn price_ratio(numerator: Decimal, denominator: Decimal) -> Option<f64> {
    (denominator > Decimal::ZERO)
        .then(|| (numerator / denominator).to_f64())
        .flatten()
}

/// Bars per day, week and month. HAR means count bars, not calendar time, so
/// closed days are skipped as everywhere else in the harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lookbacks {
    pub day: usize,
    pub week: usize,
    pub month: usize,
}

impl Lookbacks {
    pub fn of(timeframe: Timeframe) -> Self {
        let day = usize::try_from(DAY_MS / timeframe.duration_ms()).expect("positive duration");
        Self {
            day,
            week: WEEK_DAYS * day,
            month: MONTH_DAYS * day,
        }
    }
}

/// `TR_j / close_{j-1}`; `None` for the first candle or a non-positive close.
pub fn relative_true_ranges(candles: &[Candle]) -> Vec<Option<f64>> {
    candles
        .iter()
        .enumerate()
        .map(|(index, candle)| {
            let close = candles[index.checked_sub(1)?].close;
            if close <= Decimal::ZERO {
                return None;
            }
            let range = (candle.high - candle.low)
                .max((candle.high - close).abs())
                .max((candle.low - close).abs());
            (range / close).to_f64()
        })
        .collect()
}

/// Minutes since local midnight in New York, divided by the timeframe.
pub fn new_york_slot(time: i64, timeframe: Timeframe) -> usize {
    let local = DateTime::from_timestamp_millis(time)
        .expect("valid candle time")
        .with_timezone(&New_York);
    let minutes = i64::from(local.hour() * 60 + local.minute());
    usize::try_from(minutes * 60_000 / timeframe.duration_ms()).expect("non-negative slot")
}

/// `ln S`: log of the mean profile over the slots of the horizon's bar opens.
pub fn seasonal_feature(profile: &[f64], opens: &[i64], timeframe: Timeframe) -> Option<f64> {
    if opens.is_empty() {
        return None;
    }
    let mean = opens
        .iter()
        .map(|open| profile.get(new_york_slot(*open, timeframe)).copied())
        .sum::<Option<f64>>()?
        / opens.len() as f64;
    positive_ln(mean)
}

/// HAR features (`ln RD`, `ln RW`, `ln RM`) of a forecast made after the last
/// candle of `history` closes: what `collect_origins` computes for the origin
/// right after `history`. `None` without a month of positive ranges.
pub fn har_features(history: &[Candle], timeframe: Timeframe) -> Option<[f64; 3]> {
    let lookbacks = Lookbacks::of(timeframe);
    if history.len() < lookbacks.month + 1 {
        return None;
    }
    let tail = &history[history.len() - lookbacks.month - 1..];
    let rtr = relative_true_ranges(tail);
    let mean = |bars: usize| -> Option<f64> {
        rtr[rtr.len() - bars..]
            .iter()
            .copied()
            .sum::<Option<f64>>()
            .map(|sum| sum / bars as f64)
    };
    Some([
        positive_ln(mean(lookbacks.day)?)?,
        positive_ln(mean(lookbacks.week)?)?,
        positive_ln(mean(lookbacks.month)?)?,
    ])
}

/// A `HAR_RANGE_SEASONAL` fit frozen for display, with the `ATR14_CAL` fit of the
/// same window for comparison. Built from a research log by
/// `scripts/research/export_range_model.py`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeModel {
    pub vol_version: String,
    pub model: String,
    pub symbol: String,
    pub timeframe: String,
    pub horizon_bars: usize,
    /// Intercept, `ln RD`, `ln RW`, `ln RM`, `ln S`.
    pub coefficients: Vec<f64>,
    /// Intercept, `ln(ATR14 / close)`.
    pub baseline_coefficients: Vec<f64>,
    /// `P(s)` by New York slot.
    pub profile: Vec<f64>,
}

/// Typical `max high - min low` over the coming horizon, in price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeForecast {
    pub range: f64,
    /// What calibrated ATR14 forecasts for the same horizon.
    pub atr_range: f64,
}

impl RangeModel {
    /// The timeframe this model forecasts, after checking its shape.
    pub fn validated_timeframe(&self) -> Result<Timeframe, String> {
        let timeframe = Timeframe::ALL
            .into_iter()
            .find(|timeframe| timeframe.as_str() == self.timeframe)
            .ok_or_else(|| format!("unknown timeframe {}", self.timeframe))?;
        let checks = [
            (self.vol_version == VOL_VERSION, "volVersion must be vol-v1"),
            (
                self.model == DISPLAY_MODEL,
                "model must be HAR_RANGE_SEASONAL",
            ),
            (self.horizon_bars == HORIZON_BARS, "horizonBars must be 24"),
            (self.coefficients.len() == 5, "coefficients need 5 values"),
            (
                self.baseline_coefficients.len() == 2,
                "baselineCoefficients need 2 values",
            ),
            (
                self.profile.len() == Lookbacks::of(timeframe).day,
                "profile needs one value per slot of the day",
            ),
            (
                self.coefficients
                    .iter()
                    .chain(&self.baseline_coefficients)
                    .chain(&self.profile)
                    .all(|value| value.is_finite()),
                "values must be finite",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, message)) => Err((*message).to_owned()),
            None => Ok(timeframe),
        }
    }

    /// Forecast made after the last candle of the session-clean `history` closes,
    /// for the bars opening at `future_opens` (exactly `horizon_bars` of them).
    /// `atr` is ATR14 at the last candle. Prices are scaled by its close, the
    /// stand-in for the next open.
    pub fn forecast(
        &self,
        history: &[Candle],
        atr: Decimal,
        future_opens: &[i64],
        timeframe: Timeframe,
    ) -> Option<RangeForecast> {
        if future_opens.len() != self.horizon_bars {
            return None;
        }
        let last = history.last()?;
        let close = last.close.to_f64().filter(|close| *close > 0.0)?;
        let [ln_rd, ln_rw, ln_rm] = har_features(history, timeframe)?;
        let ln_season = seasonal_feature(&self.profile, future_opens, timeframe)?;
        let x_atr = positive_ln(price_ratio(atr, last.close)?)?;
        let linear = |weights: &[f64], values: &[f64]| -> f64 {
            weights.iter().zip(values).map(|(w, v)| w * v).sum()
        };
        let range = linear(&self.coefficients, &[1.0, ln_rd, ln_rw, ln_rm, ln_season]).exp();
        let atr_range = linear(&self.baseline_coefficients, &[1.0, x_atr]).exp();
        Some(RangeForecast {
            range: range * close,
            atr_range: atr_range * close,
        })
    }
}

// --- Scheduled US releases (vol-v3). See
// `docs/superpowers/specs/2026-11-08-event-aware-range-forecast-design.md`.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventKind {
    Nfp,
    Cpi,
    Fomc,
}

impl EventKind {
    pub const ALL: [Self; 3] = [Self::Nfp, Self::Cpi, Self::Fomc];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nfp => "NFP",
            Self::Cpi => "CPI",
            Self::Fomc => "FOMC",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "NFP" => Some(Self::Nfp),
            "CPI" => Some(Self::Cpi),
            "FOMC" => Some(Self::Fomc),
            _ => None,
        }
    }
}

/// One scheduled event; a point when `start == end`, else an interval (FOMC).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacroEvent {
    pub kind: EventKind,
    pub start: i64,
    pub end: i64,
}

/// `config/macro-events.json` as written by `scripts/research/build_macro_events.py`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct MacroEventsFile {
    pub version: String,
    pub events: Vec<MacroEventRow>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct MacroEventRow {
    #[serde(rename = "type")]
    pub kind: String,
    pub start: String,
    pub end: String,
}

impl MacroEventsFile {
    /// Events in milliseconds, sorted by start; any unknown type or bad time is an error.
    pub fn events(&self) -> Result<Vec<MacroEvent>, String> {
        let millis = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .map(|time| time.timestamp_millis())
                .map_err(|error| format!("bad event time {text}: {error}"))
        };
        let mut events = self
            .events
            .iter()
            .map(|row| {
                let kind = EventKind::parse(&row.kind)
                    .ok_or_else(|| format!("unknown event type {}", row.kind))?;
                let (start, end) = (millis(&row.start)?, millis(&row.end)?);
                if end < start {
                    return Err(format!("event ends before it starts: {}", row.start));
                }
                Ok(MacroEvent { kind, start, end })
            })
            .collect::<Result<Vec<_>, String>>()?;
        events.sort_by_key(|event| (event.start, event.end));
        Ok(events)
    }
}

/// Events touching `[from, to]`: points inside it, or intervals overlapping it.
/// `events` must be sorted by start, as `MacroEventsFile::events` returns them.
pub fn events_between(events: &[MacroEvent], from: i64, to: i64) -> Vec<&MacroEvent> {
    let longest = events
        .iter()
        .map(|event| event.end - event.start)
        .max()
        .unwrap_or(0);
    let first = events.partition_point(|event| event.start < from - longest);
    let last = events.partition_point(|event| event.start <= to);
    events[first..last]
        .iter()
        .filter(|event| event.end >= from)
        .collect()
}

/// `d_nfp`, `d_cpi`, `d_fomc` for the horizon `[from, to]`, in `EventKind::ALL` order.
pub fn event_terms_between(events: &[MacroEvent], from: i64, to: i64) -> [f64; 3] {
    let mut terms = [0.0; 3];
    for event in events_between(events, from, to) {
        let slot = EventKind::ALL
            .iter()
            .position(|kind| *kind == event.kind)
            .expect("known kind");
        terms[slot] = 1.0;
    }
    terms
}
