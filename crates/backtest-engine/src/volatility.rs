//! Range forecast benchmark (`vol-v1`): does a HAR forecast of the next 24 bars'
//! range, with or without an intraday-periodicity term, beat the app's ATR14?
//! Research only; the formulas the app reuses live in
//! `analysis_core::range_forecast`. See
//! `docs/superpowers/specs/2026-11-08-gold-range-forecast-benchmark-design.md`.

use crate::edge::{ResearchWindow, ResearchWindows};
use chrono::{DateTime, Datelike};
use market_domain::{Candle, Timeframe};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::collections::BTreeSet;

/// Shared with the app's display-only forecast; one definition for both.
pub use analysis_core::range_forecast::{
    HORIZON_BARS, Lookbacks, RangeForecast, RangeModel, VOL_VERSION, har_features, new_york_slot,
    relative_true_ranges, seasonal_feature,
};

/// Wilder ATR period of the baseline, the ATR the app uses.
pub const ATR_PERIOD: usize = 14;
/// A slot seen in fewer train origins than this keeps a neutral profile of 1.
pub const MIN_SLOT_ORIGINS: usize = 30;
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
    profile_of(
        origins
            .iter()
            .filter(|origin| origin.window == ResearchWindow::Train),
        slot_count,
    )
}

/// `P(s)` over the given origins, the same rule as `seasonal_profile`.
fn profile_of<'a>(origins: impl IntoIterator<Item = &'a Origin>, slot_count: usize) -> Vec<f64> {
    let mut sums = vec![0.0; slot_count];
    let mut counts = vec![0_usize; slot_count];
    for origin in origins {
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
        let opens: Vec<i64> = candles[origin.index..origin.index + HORIZON_BARS]
            .iter()
            .map(|candle| candle.open_time)
            .collect();
        match seasonal_feature(profile, &opens, timeframe) {
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

// --- vol-v2: band, refit and leave-one-month-out. See
// `docs/superpowers/specs/2026-11-08-range-forecast-vault-check-and-band-design.md`.

/// Percentile by linear interpolation between order statistics (R type 7,
/// numpy's default). `None` for an empty slice or a probability outside 0..=1.
pub fn quantile(values: &[f64], probability: f64) -> Option<f64> {
    if values.is_empty() || !(0.0..=1.0).contains(&probability) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = (sorted.len() - 1) as f64 * probability;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    Some(sorted[lower] + (position - lower as f64) * (sorted[upper] - sorted[lower]))
}

/// A `HAR_RANGE_SEASONAL` fit with its own profile, and the `ATR14_CAL` fit of
/// the same origins.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeasonalFit {
    pub coefficients: Vec<f64>,
    pub baseline_coefficients: Vec<f64>,
    pub profile: Vec<f64>,
    pub origins: usize,
}

fn horizon_opens(candles: &[Candle], origin: &Origin) -> Vec<i64> {
    candles[origin.index..origin.index + HORIZON_BARS]
        .iter()
        .map(|candle| candle.open_time)
        .collect()
}

/// Fits the profile and both models on `members` only, the way `evaluate` fits
/// them on train. Members whose profile mean is not positive are left out.
pub fn fit_seasonal(
    candles: &[Candle],
    origins: &[Origin],
    members: &[usize],
    timeframe: Timeframe,
) -> Option<SeasonalFit> {
    let profile = profile_of(
        members.iter().map(|member| &origins[*member]),
        Lookbacks::of(timeframe).day,
    );
    let (mut rows, mut baseline_rows, mut targets) = (Vec::new(), Vec::new(), Vec::new());
    for member in members {
        let origin = &origins[*member];
        let Some(ln_season) =
            seasonal_feature(&profile, &horizon_opens(candles, origin), timeframe)
        else {
            continue;
        };
        rows.push(vec![
            1.0,
            origin.ln_rd,
            origin.ln_rw,
            origin.ln_rm,
            ln_season,
        ]);
        baseline_rows.push(vec![1.0, origin.x_atr]);
        targets.push(origin.target);
    }
    Some(SeasonalFit {
        coefficients: least_squares(&rows, &targets)?,
        baseline_coefficients: least_squares(&baseline_rows, &targets)?,
        profile,
        origins: rows.len(),
    })
}

impl SeasonalFit {
    /// Log residual `y - yhat` of one origin under this fit.
    pub fn residual(
        &self,
        candles: &[Candle],
        origin: &Origin,
        timeframe: Timeframe,
    ) -> Option<f64> {
        let ln_season =
            seasonal_feature(&self.profile, &horizon_opens(candles, origin), timeframe)?;
        let forecast: f64 = [1.0, origin.ln_rd, origin.ln_rw, origin.ln_rm, ln_season]
            .iter()
            .zip(&self.coefficients)
            .map(|(value, weight)| value * weight)
            .sum();
        Some(origin.target - forecast)
    }
}

/// The central 80% band of log residuals.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Band {
    pub fit: &'static str,
    pub q10: f64,
    pub q90: f64,
    pub origins: usize,
}

impl Band {
    fn of(fit: &'static str, residuals: &[f64]) -> Option<Self> {
        Some(Self {
            fit,
            q10: quantile(residuals, 0.10)?,
            q90: quantile(residuals, 0.90)?,
            origins: residuals.len(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BandReport {
    /// The live model's fit on train; it must equal the frozen model file.
    pub live_fit: SeasonalFit,
    /// Its residuals on every origin from `validation_start` to the vault.
    pub live_band: Band,
    /// The refit on every origin before the vault.
    pub refit: SeasonalFit,
    /// Leave-one-month-out residuals of the refit over the same months.
    pub refit_band: Band,
}

/// UTC calendar month as `year * 12 + month0`.
fn utc_month(time: i64) -> i64 {
    let date = DateTime::from_timestamp_millis(time).expect("valid candle time");
    i64::from(date.year()) * 12 + i64::from(date.month0())
}

/// Band quantiles of the live model, the refit and its leave-one-month-out band.
/// Only origins inside the research windows exist, so no vault candle is a target.
pub fn band_report(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    ready_at_close: &[bool],
    windows: &ResearchWindows,
    timeframe: Timeframe,
) -> Result<Option<BandReport>, VolError> {
    let origins = collect_origins(candles, atr, ready_at_close, windows, timeframe)?.origins;
    let open = |member: usize| candles[origins[member].index].open_time;
    let all: Vec<usize> = (0..origins.len()).collect();
    let train: Vec<usize> = all
        .iter()
        .copied()
        .filter(|member| origins[*member].window == ResearchWindow::Train)
        .collect();
    let banded: Vec<usize> = all
        .iter()
        .copied()
        .filter(|member| open(*member) >= windows.validation_start)
        .collect();
    let residuals = |fit: &SeasonalFit, members: &[usize]| -> Vec<f64> {
        members
            .iter()
            .filter_map(|member| fit.residual(candles, &origins[*member], timeframe))
            .collect()
    };

    let Some(live_fit) = fit_seasonal(candles, &origins, &train, timeframe) else {
        return Ok(None);
    };
    let Some(live_band) = Band::of("train", &residuals(&live_fit, &banded)) else {
        return Ok(None);
    };
    let Some(refit) = fit_seasonal(candles, &origins, &all, timeframe) else {
        return Ok(None);
    };

    // Each month of the band is predicted by a refit that saw no target bar in it.
    let months: BTreeSet<i64> = banded
        .iter()
        .map(|member| utc_month(open(*member)))
        .collect();
    let mut left_out = Vec::new();
    for month in months {
        let touches = |member: &usize| {
            let origin = &origins[*member];
            utc_month(candles[origin.index].open_time) == month
                || utc_month(candles[origin.index + HORIZON_BARS - 1].open_time) == month
        };
        let fit_members: Vec<usize> = all.iter().copied().filter(|m| !touches(m)).collect();
        let Some(fit) = fit_seasonal(candles, &origins, &fit_members, timeframe) else {
            return Ok(None);
        };
        let scored: Vec<usize> = banded
            .iter()
            .copied()
            .filter(|member| utc_month(open(*member)) == month)
            .collect();
        left_out.extend(residuals(&fit, &scored));
    }
    let Some(refit_band) = Band::of("refit-leave-one-month-out", &left_out) else {
        return Ok(None);
    };
    Ok(Some(BandReport {
        live_fit,
        live_band,
        refit,
        refit_band,
    }))
}

// --- Vault check, laptop only (Parts 1-3 of the vol-v2 spec).

/// Windows that put every origin from `vault_start` on into one window, so
/// `collect_origins` applies the vol-v1 eligibility to the vault unchanged.
/// Targets cannot pass the loaded history, which ends at the check's cutoff.
pub fn vault_windows(vault_start: i64) -> ResearchWindows {
    ResearchWindows {
        train_start: vault_start,
        validation_start: i64::MAX - 2,
        test_start: i64::MAX - 1,
        vault_start: i64::MAX,
    }
}

/// UTC dates with at least one session-clean candle opening in the vault and
/// closing by `cutoff`: the trading-day count of the 60-day rule.
pub fn vault_trading_days(candles: &[Candle], vault_start: i64, cutoff: i64) -> usize {
    candles
        .iter()
        .filter(|candle| candle.open_time >= vault_start && candle.close_time <= cutoff)
        .map(|candle| candle.open_time.div_euclid(DAY_MS))
        .collect::<BTreeSet<_>>()
        .len()
}

/// Log residuals of a frozen model and of its ATR14 baseline on vault origins.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultScores {
    pub origins: usize,
    pub open_times: Vec<i64>,
    pub residuals: Vec<f64>,
    pub baseline_residuals: Vec<f64>,
}

/// Scores `model` exactly as frozen: its own profile and coefficients, no refit.
pub fn vault_scores(
    candles: &[Candle],
    origins: &[Origin],
    model: &RangeModel,
    timeframe: Timeframe,
) -> VaultScores {
    let mut scores = VaultScores {
        origins: 0,
        open_times: Vec::new(),
        residuals: Vec::new(),
        baseline_residuals: Vec::new(),
    };
    for origin in origins {
        let Some(ln_season) =
            seasonal_feature(&model.profile, &horizon_opens(candles, origin), timeframe)
        else {
            continue;
        };
        let linear = |weights: &[f64], values: &[f64]| -> f64 {
            weights.iter().zip(values).map(|(w, v)| w * v).sum()
        };
        let forecast = linear(
            &model.coefficients,
            &[1.0, origin.ln_rd, origin.ln_rw, origin.ln_rm, ln_season],
        );
        let baseline = linear(&model.baseline_coefficients, &[1.0, origin.x_atr]);
        scores.origins += 1;
        scores.open_times.push(candles[origin.index].open_time);
        scores.residuals.push(origin.target - forecast);
        scores.baseline_residuals.push(origin.target - baseline);
    }
    scores
}

// --- vol-v3: scheduled-event terms. See
// `docs/superpowers/specs/2026-11-08-event-aware-range-forecast-design.md`.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventKind {
    Nfp,
    Cpi,
    Fomc,
}

impl EventKind {
    pub const ALL: [Self; 3] = [Self::Nfp, Self::Cpi, Self::Fomc];

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

/// `d_nfp`, `d_cpi`, `d_fomc` of one origin: 1 when an event of that kind touches
/// the horizon from the open of bar `index` to the close of bar `index + H - 1`.
pub fn event_terms(candles: &[Candle], origin: &Origin, events: &[MacroEvent]) -> [f64; 3] {
    let from = candles[origin.index].open_time;
    let to = candles[origin.index + HORIZON_BARS - 1].close_time;
    let longest = events
        .iter()
        .map(|event| event.end - event.start)
        .max()
        .unwrap_or(0);
    let first = events.partition_point(|event| event.start < from - longest);
    let last = events.partition_point(|event| event.start <= to);
    let mut terms = [0.0; 3];
    for event in &events[first..last] {
        if event.end >= from {
            let slot = EventKind::ALL
                .iter()
                .position(|kind| *kind == event.kind)
                .expect("known kind");
            terms[slot] = 1.0;
        }
    }
    terms
}

/// Per window: the vol-v3 candidate against `HAR_RANGE_SEASONAL`, split into
/// event horizons (`E`) and the rest (`N`).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventLine {
    pub vol_version: &'static str,
    pub model: &'static str,
    pub baseline: &'static str,
    pub window: ResearchWindow,
    /// `OK`, `NO_FIT` or `EMPTY`.
    pub status: &'static str,
    pub origins_event: usize,
    pub origins_other: usize,
    /// Horizons touched per kind, in `EventKind::ALL` order.
    pub horizons_by_kind: [usize; 3],
    pub mse_baseline_event: Option<f64>,
    pub mse_candidate_event: Option<f64>,
    pub mse_baseline_other: Option<f64>,
    pub mse_candidate_other: Option<f64>,
    /// Baseline minus candidate squared error on `E`, mean per UTC day, day order.
    pub daily_loss_diff_event: Vec<f64>,
    /// Candidate fit on train: HAR_RANGE_SEASONAL terms, then d_nfp, d_cpi, d_fomc.
    pub coefficients: Vec<f64>,
    pub baseline_coefficients: Vec<f64>,
}

/// Fits the baseline and the candidate on train exactly like vol-v1 and scores
/// every window out of sample.
pub fn evaluate_events(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    ready_at_close: &[bool],
    windows: &ResearchWindows,
    timeframe: Timeframe,
    events: &[MacroEvent],
) -> Result<Vec<EventLine>, VolError> {
    let mut origins = collect_origins(candles, atr, ready_at_close, windows, timeframe)?;
    let profile = seasonal_profile(&origins.origins, Lookbacks::of(timeframe).day);
    apply_profile(candles, &mut origins, &profile, timeframe);
    let origins = origins.origins;
    let terms: Vec<[f64; 3]> = origins
        .iter()
        .map(|origin| event_terms(candles, origin, events))
        .collect();
    let base_row = |origin: &Origin| {
        vec![
            1.0,
            origin.ln_rd,
            origin.ln_rw,
            origin.ln_rm,
            origin.ln_season,
        ]
    };
    let candidate_row = |position: usize| {
        let mut row = base_row(&origins[position]);
        row.extend_from_slice(&terms[position]);
        row
    };
    let train: Vec<usize> = (0..origins.len())
        .filter(|position| origins[*position].window == ResearchWindow::Train)
        .collect();
    let targets: Vec<f64> = train.iter().map(|p| origins[*p].target).collect();
    let baseline = least_squares(
        &train
            .iter()
            .map(|p| base_row(&origins[*p]))
            .collect::<Vec<_>>(),
        &targets,
    );
    let candidate = least_squares(
        &train.iter().map(|p| candidate_row(*p)).collect::<Vec<_>>(),
        &targets,
    );
    let dot = |weights: &[f64], values: &[f64]| -> f64 {
        weights.iter().zip(values).map(|(w, v)| w * v).sum()
    };

    let mut lines = Vec::new();
    for window in ResearchWindow::ALL {
        let members: Vec<usize> = (0..origins.len())
            .filter(|position| origins[*position].window == window)
            .collect();
        let is_event = |position: &usize| terms[*position].iter().any(|term| *term > 0.0);
        let event: Vec<usize> = members.iter().copied().filter(is_event).collect();
        let other: Vec<usize> = members.iter().copied().filter(|p| !is_event(p)).collect();
        let mut horizons_by_kind = [0_usize; 3];
        for position in &members {
            for (slot, term) in terms[*position].iter().enumerate() {
                horizons_by_kind[slot] += usize::from(*term > 0.0);
            }
        }
        let mut line = EventLine {
            vol_version: "vol-v3",
            model: "HAR_RANGE_SEASONAL_EVENTS",
            baseline: VolModel::HarRangeSeasonal.name(),
            window,
            status: "NO_FIT",
            origins_event: event.len(),
            origins_other: other.len(),
            horizons_by_kind,
            mse_baseline_event: None,
            mse_candidate_event: None,
            mse_baseline_other: None,
            mse_candidate_other: None,
            daily_loss_diff_event: Vec::new(),
            coefficients: candidate.clone().unwrap_or_default(),
            baseline_coefficients: baseline.clone().unwrap_or_default(),
        };
        if let (Some(base), Some(cand)) = (&baseline, &candidate) {
            let base_error = |p: usize| origins[p].target - dot(base, &base_row(&origins[p]));
            let cand_error = |p: usize| origins[p].target - dot(cand, &candidate_row(p));
            let mse = |set: &[usize], error: &dyn Fn(usize) -> f64| -> Option<f64> {
                (!set.is_empty())
                    .then(|| set.iter().map(|p| error(*p).powi(2)).sum::<f64>() / set.len() as f64)
            };
            line.status = if members.is_empty() { "EMPTY" } else { "OK" };
            line.mse_baseline_event = mse(&event, &base_error);
            line.mse_candidate_event = mse(&event, &cand_error);
            line.mse_baseline_other = mse(&other, &base_error);
            line.mse_candidate_other = mse(&other, &cand_error);
            line.daily_loss_diff_event = daily_means(&origins, &event, |p| {
                base_error(p).powi(2) - cand_error(p).powi(2)
            });
        }
        lines.push(line);
    }
    Ok(lines)
}
