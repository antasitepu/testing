//! Range forecast benchmark (`vol-v1`): does a HAR forecast of the next 24 bars'
//! range, with or without an intraday-periodicity term, beat the app's ATR14?
//! Research only; nothing here reaches a plan or the live system. See
//! `docs/superpowers/specs/2026-11-08-gold-range-forecast-benchmark-design.md`.

use crate::edge::{ResearchWindow, ResearchWindows};
use chrono::{DateTime, Timelike};
use chrono_tz::America::New_York;
use market_domain::{Candle, Timeframe};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

pub const VOL_VERSION: &str = "vol-v1";
/// Forecast horizon in bars, the `edge-v1` time stop.
pub const HORIZON_BARS: usize = 24;
/// Wilder ATR period of the baseline, the ATR the app uses.
pub const ATR_PERIOD: usize = 14;
/// A slot seen in fewer train origins than this keeps a neutral profile of 1.
pub const MIN_SLOT_ORIGINS: usize = 30;
const WEEK_DAYS: usize = 5;
const MONTH_DAYS: usize = 22;
const DAY_MS: i64 = 86_400_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum VolError {
    #[error("ATR values must cover every candle")]
    AtrLengthMismatch,
    #[error("readiness must cover every candle")]
    ReadinessLengthMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolModel {
    /// Baseline: ATR14 with the best linear scaling on train.
    Atr14Cal,
    HarRange,
    HarRangeSeasonal,
}

impl VolModel {
    pub const ALL: [Self; 3] = [Self::Atr14Cal, Self::HarRange, Self::HarRangeSeasonal];

    pub fn name(self) -> &'static str {
        match self {
            Self::Atr14Cal => "ATR14_CAL",
            Self::HarRange => "HAR_RANGE",
            Self::HarRangeSeasonal => "HAR_RANGE_SEASONAL",
        }
    }

    pub fn is_baseline(self) -> bool {
        self == Self::Atr14Cal
    }

    /// Regressors with a leading intercept.
    fn regressors(self, origin: &Origin) -> Vec<f64> {
        match self {
            Self::Atr14Cal => vec![1.0, origin.x_atr],
            Self::HarRange => vec![1.0, origin.ln_rd, origin.ln_rw, origin.ln_rm],
            Self::HarRangeSeasonal => vec![
                1.0,
                origin.ln_rd,
                origin.ln_rw,
                origin.ln_rm,
                origin.ln_season,
            ],
        }
    }
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

/// One forecast, made after bar `index - 1` closes, for bars `index..index + H`.
#[derive(Clone, Debug, PartialEq)]
pub struct Origin {
    pub index: usize,
    pub window: ResearchWindow,
    /// UTC day of the origin's open: the Diebold-Mariano block.
    pub day: i64,
    /// `ln((max high - min low) / open_index)` over the horizon.
    pub target: f64,
    pub x_atr: f64,
    pub ln_rd: f64,
    pub ln_rw: f64,
    pub ln_rm: f64,
    /// New York slot of bar `index`.
    pub slot: usize,
    /// `rtr_index / RM`, this origin's contribution to the profile.
    pub profile_ratio: f64,
    /// `ln S`, set once the profile is fitted on train.
    pub ln_season: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Origins {
    pub origins: Vec<Origin>,
    /// Eligible origins with a non-positive range, ATR, HAR mean or profile,
    /// in `ResearchWindow::ALL` order.
    pub skipped: [usize; 3],
}

fn window_slot(window: ResearchWindow) -> usize {
    ResearchWindow::ALL
        .iter()
        .position(|candidate| *candidate == window)
        .expect("known window")
}

fn positive_ln(value: f64) -> Option<f64> {
    (value > 0.0 && value.is_finite()).then(|| value.ln())
}

fn price_ratio(numerator: Decimal, denominator: Decimal) -> Option<f64> {
    (denominator > Decimal::ZERO)
        .then(|| (numerator / denominator).to_f64())
        .flatten()
}

/// Eligible origins with their target and features; `ln_season` is left at 0.
/// Eligible: a full month of history before `index`, bars `index - 1` and
/// `index + H - 1` ready at close, and the whole target inside one window.
pub fn collect_origins(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    ready_at_close: &[bool],
    windows: &ResearchWindows,
    timeframe: Timeframe,
) -> Result<Origins, VolError> {
    if atr.len() != candles.len() {
        return Err(VolError::AtrLengthMismatch);
    }
    if ready_at_close.len() != candles.len() {
        return Err(VolError::ReadinessLengthMismatch);
    }
    let lookbacks = Lookbacks::of(timeframe);
    let rtr = relative_true_ranges(candles);
    let mut sums = vec![0.0; candles.len() + 1];
    let mut missing = vec![0_usize; candles.len() + 1];
    for (index, value) in rtr.iter().enumerate() {
        sums[index + 1] = sums[index] + value.unwrap_or(0.0);
        missing[index + 1] = missing[index] + usize::from(value.is_none());
    }
    // Mean of `rtr` over the `bars` candles before `end`.
    let trailing_mean = |end: usize, bars: usize| -> Option<f64> {
        let start = end - bars;
        (missing[end] == missing[start]).then(|| (sums[end] - sums[start]) / bars as f64)
    };

    let mut result = Origins::default();
    for index in (lookbacks.month + 1)..candles.len() {
        let end = index + HORIZON_BARS - 1;
        if end >= candles.len() {
            break;
        }
        if !ready_at_close[index - 1] || !ready_at_close[end] {
            continue;
        }
        let Some(window) = windows.window_of(candles[index].open_time) else {
            continue;
        };
        if windows.window_of(candles[end].open_time) != Some(window) {
            continue;
        }
        let origin = (|| {
            let span = &candles[index..=end];
            let high = span.iter().map(|candle| candle.high).max()?;
            let low = span.iter().map(|candle| candle.low).min()?;
            let rm = trailing_mean(index, lookbacks.month)?;
            Some(Origin {
                index,
                window,
                day: candles[index].open_time.div_euclid(DAY_MS),
                target: positive_ln(price_ratio(high - low, candles[index].open)?)?,
                x_atr: positive_ln(price_ratio(atr[index - 1]?, candles[index - 1].close)?)?,
                ln_rd: positive_ln(trailing_mean(index, lookbacks.day)?)?,
                ln_rw: positive_ln(trailing_mean(index, lookbacks.week)?)?,
                ln_rm: positive_ln(rm)?,
                slot: new_york_slot(candles[index].open_time, timeframe),
                profile_ratio: rtr[index]? / rm,
                ln_season: 0.0,
            })
        })();
        match origin {
            Some(origin) => result.origins.push(origin),
            None => result.skipped[window_slot(window)] += 1,
        }
    }
    Ok(result)
}

/// `P(s)`: mean `profile_ratio` of train origins in slot `s` over the same mean
/// of all train origins. A slot with fewer than `MIN_SLOT_ORIGINS` keeps 1.
pub fn seasonal_profile(origins: &[Origin], slot_count: usize) -> Vec<f64> {
    let mut sums = vec![0.0; slot_count];
    let mut counts = vec![0_usize; slot_count];
    for origin in origins
        .iter()
        .filter(|origin| origin.window == ResearchWindow::Train)
    {
        sums[origin.slot] += origin.profile_ratio;
        counts[origin.slot] += 1;
    }
    let total: usize = counts.iter().sum();
    if total == 0 {
        return vec![1.0; slot_count];
    }
    let overall = sums.iter().sum::<f64>() / total as f64;
    sums.iter()
        .zip(&counts)
        .map(|(sum, &count)| {
            if count < MIN_SLOT_ORIGINS || overall <= 0.0 {
                1.0
            } else {
                sum / count as f64 / overall
            }
        })
        .collect()
}

/// Sets `ln S` from the slots of the horizon's bars; an origin whose mean
/// profile is not positive moves to `skipped`.
pub fn apply_profile(
    candles: &[Candle],
    origins: &mut Origins,
    profile: &[f64],
    timeframe: Timeframe,
) {
    let mut kept = Vec::with_capacity(origins.origins.len());
    for mut origin in origins.origins.drain(..) {
        let mean = candles[origin.index..origin.index + HORIZON_BARS]
            .iter()
            .map(|candle| profile[new_york_slot(candle.open_time, timeframe)])
            .sum::<f64>()
            / HORIZON_BARS as f64;
        match positive_ln(mean) {
            Some(ln_season) => {
                origin.ln_season = ln_season;
                kept.push(origin);
            }
            None => origins.skipped[window_slot(origin.window)] += 1,
        }
    }
    origins.origins = kept;
}

/// Least squares through the normal equations, solved by Gaussian elimination
/// with partial pivoting. `None` with fewer rows than columns or a singular system.
pub fn least_squares(rows: &[Vec<f64>], targets: &[f64]) -> Option<Vec<f64>> {
    let columns = rows.first()?.len();
    if rows.len() < columns || rows.len() != targets.len() {
        return None;
    }
    // Augmented [X'X | X'y].
    let mut system = vec![vec![0.0; columns + 1]; columns];
    for (row, target) in rows.iter().zip(targets) {
        for left in 0..columns {
            for right in 0..columns {
                system[left][right] += row[left] * row[right];
            }
            system[left][columns] += row[left] * target;
        }
    }
    let scale = (0..columns)
        .map(|index| system[index][index].abs())
        .fold(0.0, f64::max);
    for pivot in 0..columns {
        let best = (pivot..columns)
            .max_by(|a, b| system[*a][pivot].abs().total_cmp(&system[*b][pivot].abs()))?;
        if system[best][pivot].abs() <= scale * 1e-12 {
            return None;
        }
        system.swap(pivot, best);
        let pivot_row = system[pivot].clone();
        for (index, row) in system.iter_mut().enumerate() {
            if index != pivot {
                let factor = row[pivot] / pivot_row[pivot];
                for (value, above) in row.iter_mut().zip(&pivot_row).skip(pivot) {
                    *value -= factor * above;
                }
            }
        }
    }
    Some(
        (0..columns)
            .map(|index| system[index][columns] / system[index][index])
            .collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolLine {
    pub vol_version: &'static str,
    pub model: &'static str,
    pub role: &'static str,
    pub window: ResearchWindow,
    /// `OK`, `NO_FIT` (no least-squares solution on train) or `EMPTY`.
    pub status: &'static str,
    pub horizon_bars: usize,
    pub origins: usize,
    pub skipped: usize,
    pub mse: Option<f64>,
    pub mae: Option<f64>,
    pub mean_error: Option<f64>,
    /// Fitted on train; intercept first.
    pub coefficients: Vec<f64>,
    /// Candidates only: baseline minus candidate squared error, averaged per
    /// UTC day of the origin, in day order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_loss_diff: Option<Vec<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VolReport {
    /// `P(s)` by New York slot, fitted on train.
    pub profile: Vec<f64>,
    pub lines: Vec<VolLine>,
}

/// Fits every model on train and scores each window out of sample.
pub fn evaluate(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    ready_at_close: &[bool],
    windows: &ResearchWindows,
    timeframe: Timeframe,
) -> Result<VolReport, VolError> {
    let mut origins = collect_origins(candles, atr, ready_at_close, windows, timeframe)?;
    let profile = seasonal_profile(&origins.origins, Lookbacks::of(timeframe).day);
    apply_profile(candles, &mut origins, &profile, timeframe);

    let train: Vec<&Origin> = origins
        .origins
        .iter()
        .filter(|origin| origin.window == ResearchWindow::Train)
        .collect();
    let targets: Vec<f64> = train.iter().map(|origin| origin.target).collect();
    // Per model: coefficients and every origin's error `y - yhat`.
    let fitted: Vec<Option<(Vec<f64>, Vec<f64>)>> = VolModel::ALL
        .iter()
        .map(|model| {
            let rows: Vec<Vec<f64>> = train
                .iter()
                .map(|origin| model.regressors(origin))
                .collect();
            let coefficients = least_squares(&rows, &targets)?;
            let errors = origins
                .origins
                .iter()
                .map(|origin| {
                    let forecast: f64 = model
                        .regressors(origin)
                        .iter()
                        .zip(&coefficients)
                        .map(|(value, weight)| value * weight)
                        .sum();
                    origin.target - forecast
                })
                .collect();
            Some((coefficients, errors))
        })
        .collect();
    let baseline_errors = fitted[0].as_ref().map(|(_, errors)| errors);

    let mut lines = Vec::new();
    for window in ResearchWindow::ALL {
        let members: Vec<usize> = (0..origins.origins.len())
            .filter(|position| origins.origins[*position].window == window)
            .collect();
        for (model, fit) in VolModel::ALL.iter().zip(&fitted) {
            let mut line = VolLine {
                vol_version: VOL_VERSION,
                model: model.name(),
                role: if model.is_baseline() {
                    "baseline"
                } else {
                    "candidate"
                },
                window,
                status: "NO_FIT",
                horizon_bars: HORIZON_BARS,
                origins: members.len(),
                skipped: origins.skipped[window_slot(window)],
                mse: None,
                mae: None,
                mean_error: None,
                coefficients: Vec::new(),
                daily_loss_diff: None,
            };
            if let Some((coefficients, errors)) = fit {
                line.coefficients = coefficients.clone();
                line.status = if members.is_empty() { "EMPTY" } else { "OK" };
                if !members.is_empty() {
                    let count = members.len() as f64;
                    line.mse =
                        Some(members.iter().map(|m| errors[*m].powi(2)).sum::<f64>() / count);
                    line.mae = Some(members.iter().map(|m| errors[*m].abs()).sum::<f64>() / count);
                    line.mean_error = Some(members.iter().map(|m| errors[*m]).sum::<f64>() / count);
                    if let (false, Some(baseline)) = (model.is_baseline(), baseline_errors) {
                        line.daily_loss_diff = Some(daily_means(&origins.origins, &members, |m| {
                            baseline[m].powi(2) - errors[m].powi(2)
                        }));
                    }
                }
            }
            lines.push(line);
        }
    }
    Ok(VolReport { profile, lines })
}

/// Mean of `value` per UTC day; `members` are in index order, so days ascend.
fn daily_means(origins: &[Origin], members: &[usize], value: impl Fn(usize) -> f64) -> Vec<f64> {
    let mut means = Vec::new();
    let mut current: Option<(i64, f64, usize)> = None;
    for member in members {
        let day = origins[*member].day;
        current = match current {
            Some((open, sum, count)) if open == day => {
                Some((open, sum + value(*member), count + 1))
            }
            Some((_, sum, count)) => {
                means.push(sum / count as f64);
                Some((day, value(*member), 1))
            }
            None => Some((day, value(*member), 1)),
        };
    }
    if let Some((_, sum, count)) = current {
        means.push(sum / count as f64);
    }
    means
}
