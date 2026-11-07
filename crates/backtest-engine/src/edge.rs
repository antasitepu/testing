//! Entry-edge benchmark (`edge-v1`).
//!
//! A plan's real entries and random entries are scored with one standardized
//! exit through the baseline execution model, so the comparison measures the
//! information in the entries alone. It runs at the baseline defaults: zero
//! spread (MID) and no fees, so entries fill at the plain open. See
//! `docs/superpowers/specs/2026-11-07-entry-edge-random-benchmark-design.md`.

use crate::execution::{
    ExecutionAdapter, ExecutionKind, OhlcConservativeExecution, SimulatedExecution,
};
use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanKind, SignalDefinition,
};
use market_domain::Candle;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const EDGE_VERSION: &str = "edge-v1";

/// Every fixed parameter of the benchmark. Changing any value after results
/// have been seen makes a new edge version and a new trial in the ledger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeParameters {
    /// Wilder ATR period on the plan timeframe, read at the signal bar `i-1`.
    pub atr_period: usize,
    /// Stop and target distance, both `stop_atr_multiple x ATR`.
    pub stop_atr_multiple: Decimal,
    /// Exit at the open of bar `i + time_stop_bars` if neither side was hit.
    pub time_stop_bars: usize,
    /// Monte Carlo repetitions per (case, window, variant).
    pub repetitions: u32,
    /// Fewer scored entries in a window than this reports `INSUFFICIENT`.
    pub minimum_entries: usize,
}

pub const EDGE_V1: EdgeParameters = EdgeParameters {
    atr_period: 14,
    stop_atr_multiple: Decimal::from_parts(15, 0, 0, false, 1),
    time_stop_bars: 24,
    repetitions: 100_000,
    minimum_entries: 30,
};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EdgeError {
    #[error("invalid research window date: {0}")]
    InvalidDate(String),
    #[error("research windows must be in ascending order")]
    WindowsOutOfOrder,
    #[error("ATR values must cover every candle")]
    AtrLengthMismatch,
    #[error("eligibility must cover every candle")]
    EligibilityLengthMismatch,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResearchWindow {
    Train,
    Validation,
    Test,
}

impl ResearchWindow {
    pub const ALL: [Self; 3] = [Self::Train, Self::Validation, Self::Test];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Validation => "validation",
            Self::Test => "test",
        }
    }
}

/// `config/research-windows.json`: UTC dates, each the first day of a window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResearchWindowDates {
    pub version: String,
    pub train_start: String,
    pub validation_start: String,
    pub test_start: String,
    pub vault_start: String,
}

/// Window boundaries in epoch milliseconds. Before `train_start` is warm-up,
/// from `vault_start` is the vault; neither belongs to a window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResearchWindows {
    pub train_start: i64,
    pub validation_start: i64,
    pub test_start: i64,
    pub vault_start: i64,
}

impl ResearchWindows {
    pub fn from_dates(dates: &ResearchWindowDates) -> Result<Self, EdgeError> {
        let windows = Self {
            train_start: utc_midnight_ms(&dates.train_start)?,
            validation_start: utc_midnight_ms(&dates.validation_start)?,
            test_start: utc_midnight_ms(&dates.test_start)?,
            vault_start: utc_midnight_ms(&dates.vault_start)?,
        };
        if windows.train_start < windows.validation_start
            && windows.validation_start < windows.test_start
            && windows.test_start < windows.vault_start
        {
            Ok(windows)
        } else {
            Err(EdgeError::WindowsOutOfOrder)
        }
    }

    pub fn window_of(&self, time: i64) -> Option<ResearchWindow> {
        if time < self.train_start || time >= self.vault_start {
            None
        } else if time < self.validation_start {
            Some(ResearchWindow::Train)
        } else if time < self.test_start {
            Some(ResearchWindow::Validation)
        } else {
            Some(ResearchWindow::Test)
        }
    }
}

/// Parses `YYYY-MM-DD` as midnight UTC in epoch milliseconds.
pub fn utc_midnight_ms(date: &str) -> Result<i64, EdgeError> {
    let invalid = || EdgeError::InvalidDate(date.to_owned());
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return Err(invalid());
    }
    let year: i64 = year.parse().map_err(|_| invalid())?;
    let month: i64 = month.parse().map_err(|_| invalid())?;
    let day: i64 = day.parse().map_err(|_| invalid())?;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month) || day < 1 || day > month_days[(month - 1) as usize] {
        return Err(invalid());
    }
    // Days from civil date (proleptic Gregorian), Howard Hinnant's algorithm.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Ok((era * 146_097 + day_of_era - 719_468) * 86_400_000)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BracketExit {
    Target,
    Stop,
    TimeStop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BracketOutcome {
    /// `(exit - entry) / distance`, signed by direction.
    pub r: f64,
    pub exit: BracketExit,
    pub ambiguous: bool,
}

/// Standardized outcome of entering at the open of each bar, per direction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BarOutcomes {
    pub long: Vec<Option<BracketOutcome>>,
    pub short: Vec<Option<BracketOutcome>>,
}

impl BarOutcomes {
    pub fn get(&self, index: usize, direction: Direction) -> Option<BracketOutcome> {
        match direction {
            Direction::Buy => self.long.get(index).copied().flatten(),
            Direction::Sell => self.short.get(index).copied().flatten(),
            Direction::None => None,
        }
    }
}

/// The synthetic order is always tagged TREND_BREAKOUT: `on_signal` runs the
/// mean-reversion pre-entry validation only for MEAN_REVERSION signals.
fn synthetic_signal(
    candles: &[Candle],
    index: usize,
    direction: Direction,
    entry: Decimal,
    distance: Decimal,
) -> SignalDefinition {
    let candle = &candles[index];
    let (stop_loss, primary_target) = match direction {
        Direction::Buy => (entry - distance, entry + distance),
        _ => (entry + distance, entry - distance),
    };
    SignalDefinition {
        signal_id: Uuid::nil(),
        symbol: candle.symbol.clone(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: candle.timeframe,
        evaluated_at: candles[index - 1].close_time,
        direction,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(entry),
        stop_loss,
        primary_target,
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::ONE,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: None,
        effective_at: candle.open_time,
        detected_at: candle.open_time,
        published_at: candle.open_time,
        release_id: Uuid::nil(),
        algorithm_version: EDGE_VERSION.to_owned(),
        parameter_version: EDGE_VERSION.to_owned(),
        created_at: candle.open_time,
        expires_at: None,
    }
}

fn time_stop_directive(candle: &Candle) -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::nil(),
        signal_id: Uuid::nil(),
        virtual_trade_id: None,
        kind: DirectiveKind::ExitAtNextOpen,
        effective_at: candle.open_time,
        price: None,
        reason: "edge time stop".to_owned(),
        source_closed_candle_time: candle.open_time - 1,
        algorithm_version: EDGE_VERSION.to_owned(),
        parameter_version: EDGE_VERSION.to_owned(),
        reverse_signal: None,
    }
}

fn first_exit(executions: &[SimulatedExecution]) -> Option<(SimulatedExecution, BracketExit)> {
    executions
        .iter()
        .find_map(|execution| match execution.kind {
            ExecutionKind::TakeProfit => Some((execution.clone(), BracketExit::Target)),
            ExecutionKind::StopLoss => Some((execution.clone(), BracketExit::Stop)),
            ExecutionKind::ExitAtNextOpen => Some((execution.clone(), BracketExit::TimeStop)),
            _ => None,
        })
}

/// Simulates one standardized bracket entered at the open of bar `index`,
/// through the baseline execution model.
pub fn simulate_bracket(
    candles: &[Candle],
    index: usize,
    direction: Direction,
    atr_at_signal: Decimal,
    parameters: &EdgeParameters,
) -> Option<BracketOutcome> {
    let horizon = parameters.time_stop_bars;
    if index == 0
        || index + horizon >= candles.len()
        || atr_at_signal <= Decimal::ZERO
        || direction == Direction::None
    {
        return None;
    }
    let distance = atr_at_signal * parameters.stop_atr_multiple;
    let mut engine = OhlcConservativeExecution::new();
    let entry = candles[index].open;
    let signal = synthetic_signal(candles, index, direction, entry, distance);
    let fill = engine
        .on_signal(&signal, &candles[index])
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::Entry)?;
    let mut exit = None;
    for candle in &candles[index..index + horizon] {
        exit = first_exit(&engine.on_candle(candle, &[]));
        if exit.is_some() {
            break;
        }
    }
    if exit.is_none() {
        let last = &candles[index + horizon];
        exit = first_exit(&engine.on_candle(last, &[time_stop_directive(last)]));
    }
    let (exit, kind) = exit?;
    let sign = if direction == Direction::Buy {
        Decimal::ONE
    } else {
        -Decimal::ONE
    };
    Some(BracketOutcome {
        r: ((exit.price - fill.price) * sign / distance).to_f64()?,
        exit: kind,
        ambiguous: exit.ambiguous_intrabar,
    })
}

/// Computes each bar's long and short outcome once; repetitions are lookups.
/// `atr[i]` is the ATR after bar `i` closed, so an entry at bar `i` uses `atr[i-1]`.
pub fn precompute_outcomes(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    parameters: &EdgeParameters,
) -> Result<BarOutcomes, EdgeError> {
    if atr.len() != candles.len() {
        return Err(EdgeError::AtrLengthMismatch);
    }
    let mut outcomes = BarOutcomes {
        long: vec![None; candles.len()],
        short: vec![None; candles.len()],
    };
    for index in 1..candles.len() {
        let Some(atr_at_signal) = atr[index - 1] else {
            continue;
        };
        outcomes.long[index] =
            simulate_bracket(candles, index, Direction::Buy, atr_at_signal, parameters);
        outcomes.short[index] =
            simulate_bracket(candles, index, Direction::Sell, atr_at_signal, parameters);
    }
    Ok(outcomes)
}

/// Bars a random entry may use: the case was ready when the signal bar `i-1`
/// closed, bar `i` lies in a research window, and both outcomes exist.
pub fn eligible_bars(
    candles: &[Candle],
    outcomes: &BarOutcomes,
    ready_at_close: &[bool],
    windows: &ResearchWindows,
) -> Result<Vec<bool>, EdgeError> {
    if ready_at_close.len() != candles.len() {
        return Err(EdgeError::EligibilityLengthMismatch);
    }
    Ok((0..candles.len())
        .map(|index| {
            index > 0
                && ready_at_close[index - 1]
                && windows.window_of(candles[index].open_time).is_some()
                && outcomes.get(index, Direction::Buy).is_some()
                && outcomes.get(index, Direction::Sell).is_some()
        })
        .collect())
}

/// A plan entry as recorded by the replay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RealEntry {
    pub entry_time: i64,
    pub entry_price: Decimal,
    pub direction: Direction,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowEntries {
    /// (scored bar index, direction)
    pub scored: Vec<(usize, Direction)>,
    pub dropped: usize,
    pub delayed_limit: usize,
}

/// Maps real entries to the bar whose open they are scored from. A fill at
/// the bar's open scores at that bar; a fill inside the bar (a limit
/// zone edge) scores from the next open, so no intrabar knowledge is used.
pub fn score_entries(
    candles: &[Candle],
    eligible: &[bool],
    entries: &[RealEntry],
    windows: &ResearchWindows,
) -> Vec<(ResearchWindow, WindowEntries)> {
    let mut by_window: Vec<(ResearchWindow, WindowEntries)> = ResearchWindow::ALL
        .into_iter()
        .map(|window| (window, WindowEntries::default()))
        .collect();
    for entry in entries {
        let Some(window) = windows.window_of(entry.entry_time) else {
            continue;
        };
        let slot = &mut by_window
            .iter_mut()
            .find(|(candidate, _)| *candidate == window)
            .expect("every window has a slot")
            .1;
        let containing = candles
            .partition_point(|candle| candle.open_time <= entry.entry_time)
            .checked_sub(1)
            .filter(|index| entry.entry_time < candles[*index].close_time);
        let Some(bar) = containing else {
            slot.dropped += 1;
            continue;
        };
        let delayed = entry.entry_price != candles[bar].open;
        let scored = if delayed { bar + 1 } else { bar };
        if !eligible.get(scored).copied().unwrap_or(false) {
            slot.dropped += 1;
            continue;
        }
        // A delayed fill can move into the next window; count it where it scores.
        let scored_window = windows
            .window_of(candles[scored].open_time)
            .expect("eligible bars lie in a window");
        let slot = &mut by_window
            .iter_mut()
            .find(|(candidate, _)| *candidate == scored_window)
            .expect("every window has a slot")
            .1;
        slot.delayed_limit += usize::from(delayed);
        slot.scored.push((scored, entry.direction));
    }
    by_window
}

/// SplitMix64 (Steele, Lea and Flood 2014): small, deterministic, no dependency.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    /// Uniform index in `0..bound` by multiply-shift; `bound` must be positive.
    pub fn below(&mut self, bound: usize) -> usize {
        ((u128::from(self.next_u64()) * bound as u128) >> 64) as usize
    }

    pub fn coin(&mut self) -> bool {
        self.next_u64() >> 63 == 1
    }
}

/// FNV-1a 64-bit hash, used to derive one seed per case, window and variant.
pub fn fnv1a64(text: &str) -> u64 {
    text.bytes().fold(0xCBF2_9CE4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// One-sided Monte Carlo p-value; never zero.
pub fn monte_carlo_p(exceedances: u64, repetitions: u32) -> f64 {
    (1.0 + exceedances as f64) / (f64::from(repetitions) + 1.0)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NullVariant {
    /// Random time, matched long/short counts; the only variant in the pass rule.
    A,
    /// Real entry times, random direction; diagnostic only.
    B,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct NullQuantiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NullSummary {
    pub null_mean: f64,
    pub null_std: f64,
    pub null_quantiles: NullQuantiles,
    pub exceedances: u64,
    pub p_value: f64,
}

fn summarize_null(mut means: Vec<f64>, observed: f64, repetitions: u32) -> NullSummary {
    let count = means.len() as f64;
    let mean = means.iter().sum::<f64>() / count;
    let variance = means
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / count;
    let exceedances = means.iter().filter(|value| **value >= observed).count() as u64;
    means.sort_by(f64::total_cmp);
    let quantile = |p: f64| means[((p * count).ceil() as usize).clamp(1, means.len()) - 1];
    NullSummary {
        null_mean: mean,
        null_std: variance.sqrt(),
        null_quantiles: NullQuantiles {
            p50: quantile(0.5),
            p95: quantile(0.95),
            p99: quantile(0.99),
            p999: quantile(0.999),
        },
        exceedances,
        p_value: monte_carlo_p(exceedances, repetitions),
    }
}

/// Null A: `buys` long and `sells` short outcomes drawn uniformly with
/// replacement from `pool` (eligible bar indices of one window).
pub fn null_summary_random_time(
    outcomes: &BarOutcomes,
    pool: &[usize],
    buys: usize,
    sells: usize,
    observed: f64,
    repetitions: u32,
    rng: &mut SplitMix64,
) -> Option<NullSummary> {
    let entries = buys + sells;
    if pool.is_empty() || entries == 0 || repetitions == 0 {
        return None;
    }
    let long: Vec<f64> = pool
        .iter()
        .map(|index| {
            outcomes
                .get(*index, Direction::Buy)
                .map(|outcome| outcome.r)
        })
        .collect::<Option<_>>()?;
    let short: Vec<f64> = pool
        .iter()
        .map(|index| {
            outcomes
                .get(*index, Direction::Sell)
                .map(|outcome| outcome.r)
        })
        .collect::<Option<_>>()?;
    let means = (0..repetitions)
        .map(|_| {
            let mut total = 0.0;
            for _ in 0..buys {
                total += long[rng.below(long.len())];
            }
            for _ in 0..sells {
                total += short[rng.below(short.len())];
            }
            total / entries as f64
        })
        .collect();
    Some(summarize_null(means, observed, repetitions))
}

/// Null B: the real scored bars, each with a fair-coin direction.
pub fn null_summary_random_direction(
    outcomes: &BarOutcomes,
    scored: &[(usize, Direction)],
    observed: f64,
    repetitions: u32,
    rng: &mut SplitMix64,
) -> Option<NullSummary> {
    if scored.is_empty() || repetitions == 0 {
        return None;
    }
    let pairs: Vec<(f64, f64)> = scored
        .iter()
        .map(|(index, _)| {
            Some((
                outcomes.get(*index, Direction::Buy)?.r,
                outcomes.get(*index, Direction::Sell)?.r,
            ))
        })
        .collect::<Option<_>>()?;
    let means = (0..repetitions)
        .map(|_| {
            pairs
                .iter()
                .map(|(long, short)| if rng.coin() { *long } else { *short })
                .sum::<f64>()
                / pairs.len() as f64
        })
        .collect();
    Some(summarize_null(means, observed, repetitions))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeStatus {
    Ok,
    Insufficient,
}

/// One `EDGE_CASE` line: a case, one window, one null variant.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWindowResult {
    pub edge_version: &'static str,
    pub window: ResearchWindow,
    pub variant: NullVariant,
    pub status: EdgeStatus,
    pub minimum_entries: usize,
    pub entries: usize,
    pub entries_buy: usize,
    pub entries_sell: usize,
    pub dropped_entries: usize,
    pub delayed_limit_entries: usize,
    pub eligible_bars: usize,
    pub observed_mean_r: Option<f64>,
    pub observed_hit_rate: Option<f64>,
    #[serde(flatten)]
    pub null: Option<NullSummary>,
    pub repetitions: u32,
}

/// Scores one case (plan x timeframe) in every window with both null variants.
/// `case_key` is `<timeframe>/<plan>/<algorithmVersion>/<parameterVersion>`.
pub fn evaluate_case(
    candles: &[Candle],
    outcomes: &BarOutcomes,
    eligible: &[bool],
    entries: &[RealEntry],
    windows: &ResearchWindows,
    case_key: &str,
    parameters: &EdgeParameters,
) -> Vec<EdgeWindowResult> {
    let scored_by_window = score_entries(candles, eligible, entries, windows);
    let mut results = Vec::with_capacity(6);
    for (window, scored) in scored_by_window {
        let pool: Vec<usize> = (0..candles.len())
            .filter(|index| {
                eligible[*index] && windows.window_of(candles[*index].open_time) == Some(window)
            })
            .collect();
        let rs: Vec<f64> = scored
            .scored
            .iter()
            .filter_map(|(index, direction)| outcomes.get(*index, *direction).map(|o| o.r))
            .collect();
        let buys = scored
            .scored
            .iter()
            .filter(|(_, direction)| *direction == Direction::Buy)
            .count();
        let sells = scored.scored.len() - buys;
        let enough = scored.scored.len() >= parameters.minimum_entries && !pool.is_empty();
        let observed = (!rs.is_empty()).then(|| rs.iter().sum::<f64>() / rs.len() as f64);
        let hit_rate = (!rs.is_empty())
            .then(|| rs.iter().filter(|r| **r > 0.0).count() as f64 / rs.len() as f64);
        for variant in [NullVariant::A, NullVariant::B] {
            let null = match (enough, observed) {
                (true, Some(observed)) => {
                    let seed = format!("{EDGE_VERSION}/{case_key}/{}/{variant:?}", window.as_str());
                    let mut rng = SplitMix64::new(fnv1a64(&seed));
                    match variant {
                        NullVariant::A => null_summary_random_time(
                            outcomes,
                            &pool,
                            buys,
                            sells,
                            observed,
                            parameters.repetitions,
                            &mut rng,
                        ),
                        NullVariant::B => null_summary_random_direction(
                            outcomes,
                            &scored.scored,
                            observed,
                            parameters.repetitions,
                            &mut rng,
                        ),
                    }
                }
                _ => None,
            };
            results.push(EdgeWindowResult {
                edge_version: EDGE_VERSION,
                window,
                variant,
                status: if null.is_some() {
                    EdgeStatus::Ok
                } else {
                    EdgeStatus::Insufficient
                },
                minimum_entries: parameters.minimum_entries,
                entries: scored.scored.len(),
                entries_buy: buys,
                entries_sell: sells,
                dropped_entries: scored.dropped,
                delayed_limit_entries: scored.delayed_limit,
                eligible_bars: pool.len(),
                observed_mean_r: observed,
                observed_hit_rate: hit_rate,
                null,
                repetitions: parameters.repetitions,
            });
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use market_domain::{Symbol, Timeframe};

    #[test]
    fn synthetic_signal_never_takes_the_mean_reversion_path() {
        let symbol = Symbol::new("XAUUSDT").unwrap();
        let candles: Vec<_> = (0..2)
            .map(|index| {
                Candle::new(
                    symbol.clone(),
                    Timeframe::M5,
                    index * 300_000,
                    index * 300_000 + 299_999,
                    Decimal::ONE,
                    Decimal::ONE,
                    Decimal::ONE,
                    Decimal::ONE,
                )
                .unwrap()
            })
            .collect();
        let signal = synthetic_signal(&candles, 1, Direction::Sell, Decimal::ONE, Decimal::ONE);
        assert_eq!(signal.plan, PlanKind::TrendBreakout);
        assert_ne!(signal.plan, PlanKind::MeanReversion);
        assert!(signal.execution_plan.is_none());
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
    }
}
