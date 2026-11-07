use analysis_domain::{Direction, FactId, FactKind};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwingParameters {
    pub left_bars: usize,
    pub right_bars: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplacementParameters {
    pub minimum_body_atr: Decimal,
    pub minimum_range_atr: Decimal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SwingKind {
    High,
    Low,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SwingLabel {
    HigherHigh,
    HigherLow,
    LowerHigh,
    LowerLow,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StructureEventKind {
    BreakOfStructure,
    ChangeOfCharacter,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmedSwing {
    pub id: FactId,
    pub kind: SwingKind,
    pub label: Option<SwingLabel>,
    pub price: Decimal,
    pub pivot_open_time: i64,
    pub confirmed_at: i64,
    pub source_start_time: i64,
    pub source_end_time: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureEvent {
    pub id: FactId,
    pub kind: StructureEventKind,
    pub direction: Direction,
    pub broken_swing_id: FactId,
    pub confirmation_candle_open_time: i64,
    pub confirmation_close: Decimal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Displacement {
    pub id: FactId,
    pub direction: Direction,
    pub candle_open_time: i64,
    pub body_atr: Decimal,
    pub range_atr: Decimal,
    pub low: Decimal,
    pub high: Decimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructureAnalysis {
    pub swings: Vec<ConfirmedSwing>,
    pub events: Vec<StructureEvent>,
}

pub fn detect_structure(
    candles: &[Candle],
    parameters: SwingParameters,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<StructureAnalysis, StructureError> {
    let (symbol, timeframe) = validate_inputs(candles, algorithm_version, parameter_version)?;
    if parameters.left_bars == 0 || parameters.right_bars == 0 {
        return Err(StructureError::InvalidParameters);
    }
    let mut swings = Vec::new();
    let mut last_high = None;
    let mut last_low = None;
    let end = candles.len().saturating_sub(parameters.right_bars);
    for pivot_index in parameters.left_bars..end {
        let pivot = &candles[pivot_index];
        let neighbors =
            &candles[pivot_index - parameters.left_bars..=pivot_index + parameters.right_bars];
        let is_high = neighbors
            .iter()
            .enumerate()
            .all(|(offset, candle)| offset == parameters.left_bars || pivot.high > candle.high);
        let is_low = neighbors
            .iter()
            .enumerate()
            .all(|(offset, candle)| offset == parameters.left_bars || pivot.low < candle.low);
        if is_high {
            let label = last_high.map(|previous| {
                if pivot.high > previous {
                    SwingLabel::HigherHigh
                } else {
                    SwingLabel::LowerHigh
                }
            });
            last_high = Some(pivot.high);
            swings.push(make_swing(
                symbol,
                timeframe,
                candles,
                pivot_index,
                parameters,
                SwingKind::High,
                label,
                u32::try_from(swings.len()).map_err(|_| StructureError::OrdinalOverflow)?,
                algorithm_version,
                parameter_version,
            ));
        }
        if is_low {
            let label = last_low.map(|previous| {
                if pivot.low > previous {
                    SwingLabel::HigherLow
                } else {
                    SwingLabel::LowerLow
                }
            });
            last_low = Some(pivot.low);
            swings.push(make_swing(
                symbol,
                timeframe,
                candles,
                pivot_index,
                parameters,
                SwingKind::Low,
                label,
                u32::try_from(swings.len()).map_err(|_| StructureError::OrdinalOverflow)?,
                algorithm_version,
                parameter_version,
            ));
        }
    }
    swings.sort_by_key(|swing| (swing.confirmed_at, swing.pivot_open_time, swing.id.ordinal));
    let events = detect_structure_events(
        symbol,
        timeframe,
        candles,
        &swings,
        algorithm_version,
        parameter_version,
    )?;
    Ok(StructureAnalysis { swings, events })
}

#[allow(clippy::too_many_arguments)]
fn make_swing(
    symbol: &Symbol,
    timeframe: Timeframe,
    candles: &[Candle],
    pivot_index: usize,
    parameters: SwingParameters,
    kind: SwingKind,
    label: Option<SwingLabel>,
    ordinal: u32,
    algorithm_version: &str,
    parameter_version: &str,
) -> ConfirmedSwing {
    let pivot = &candles[pivot_index];
    let confirmation = &candles[pivot_index + parameters.right_bars];
    ConfirmedSwing {
        id: FactId {
            symbol: symbol.clone(),
            timeframe,
            kind: match kind {
                SwingKind::High => FactKind::SwingHigh,
                SwingKind::Low => FactKind::SwingLow,
            },
            confirmed_at: confirmation.close_time,
            ordinal,
            algorithm_version: algorithm_version.to_owned(),
            parameter_version: parameter_version.to_owned(),
        },
        kind,
        label,
        price: match kind {
            SwingKind::High => pivot.high,
            SwingKind::Low => pivot.low,
        },
        pivot_open_time: pivot.open_time,
        confirmed_at: confirmation.close_time,
        source_start_time: candles[pivot_index - parameters.left_bars].open_time,
        source_end_time: confirmation.close_time,
    }
}

fn detect_structure_events(
    symbol: &Symbol,
    timeframe: Timeframe,
    candles: &[Candle],
    swings: &[ConfirmedSwing],
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<StructureEvent>, StructureError> {
    let mut events = Vec::new();
    let mut latest_high: Option<&ConfirmedSwing> = None;
    let mut latest_low: Option<&ConfirmedSwing> = None;
    let mut next_swing = 0;
    let mut broken = HashSet::new();
    let mut prior_direction = None;

    for candle in candles {
        while next_swing < swings.len() && swings[next_swing].confirmed_at <= candle.close_time {
            let swing = &swings[next_swing];
            match swing.kind {
                SwingKind::High => latest_high = Some(swing),
                SwingKind::Low => latest_low = Some(swing),
            }
            next_swing += 1;
        }
        let broken_swing = if let Some(high) = latest_high
            && candle.close > high.price
            && !broken.contains(&high.id)
        {
            Some((high, Direction::Buy))
        } else if let Some(low) = latest_low
            && candle.close < low.price
            && !broken.contains(&low.id)
        {
            Some((low, Direction::Sell))
        } else {
            None
        };
        let Some((swing, direction)) = broken_swing else {
            continue;
        };
        broken.insert(swing.id.clone());
        let kind = if prior_direction.is_some_and(|prior| prior != direction) {
            StructureEventKind::ChangeOfCharacter
        } else {
            StructureEventKind::BreakOfStructure
        };
        prior_direction = Some(direction);
        events.push(StructureEvent {
            id: FactId {
                symbol: symbol.clone(),
                timeframe,
                kind: FactKind::Structure,
                confirmed_at: candle.close_time,
                ordinal: u32::try_from(events.len())
                    .map_err(|_| StructureError::OrdinalOverflow)?,
                algorithm_version: algorithm_version.to_owned(),
                parameter_version: parameter_version.to_owned(),
            },
            kind,
            direction,
            broken_swing_id: swing.id.clone(),
            confirmation_candle_open_time: candle.open_time,
            confirmation_close: candle.close,
        });
    }
    Ok(events)
}

pub fn detect_displacements(
    candles: &[Candle],
    atr: Decimal,
    parameters: DisplacementParameters,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<Displacement>, StructureError> {
    if atr <= Decimal::ZERO {
        return Err(StructureError::InvalidParameters);
    }
    scan_displacements(
        candles,
        |_| Some(atr),
        parameters,
        algorithm_version,
        parameter_version,
    )
}

/// Scores every candle against the ATR known at its own close, so appending newer
/// candles never repaints historical displacements. Candles without ATR are skipped.
pub fn detect_displacements_with_atr(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    parameters: DisplacementParameters,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<Displacement>, StructureError> {
    if atr.len() != candles.len() {
        return Err(StructureError::InvalidParameters);
    }
    scan_displacements(
        candles,
        |index| atr[index].filter(|value| *value > Decimal::ZERO),
        parameters,
        algorithm_version,
        parameter_version,
    )
}

fn scan_displacements(
    candles: &[Candle],
    atr_at: impl Fn(usize) -> Option<Decimal>,
    parameters: DisplacementParameters,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<Displacement>, StructureError> {
    let (symbol, timeframe) = validate_inputs(candles, algorithm_version, parameter_version)?;
    if parameters.minimum_body_atr <= Decimal::ZERO || parameters.minimum_range_atr <= Decimal::ZERO
    {
        return Err(StructureError::InvalidParameters);
    }
    let mut displacements = Vec::new();
    for (index, candle) in candles.iter().enumerate() {
        let Some(atr) = atr_at(index) else {
            continue;
        };
        let body_atr = (candle.close - candle.open).abs() / atr;
        let range_atr = (candle.high - candle.low) / atr;
        if body_atr < parameters.minimum_body_atr
            || range_atr < parameters.minimum_range_atr
            || candle.close == candle.open
        {
            continue;
        }
        displacements.push(Displacement {
            id: FactId {
                symbol: symbol.clone(),
                timeframe,
                kind: FactKind::Displacement,
                confirmed_at: candle.close_time,
                ordinal: u32::try_from(displacements.len())
                    .map_err(|_| StructureError::OrdinalOverflow)?,
                algorithm_version: algorithm_version.to_owned(),
                parameter_version: parameter_version.to_owned(),
            },
            direction: if candle.close > candle.open {
                Direction::Buy
            } else {
                Direction::Sell
            },
            candle_open_time: candle.open_time,
            body_atr,
            range_atr,
            low: candle.low,
            high: candle.high,
        });
    }
    Ok(displacements)
}

fn validate_inputs<'a>(
    candles: &'a [Candle],
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<(&'a Symbol, Timeframe), StructureError> {
    let first = candles.first().ok_or(StructureError::EmptyCandles)?;
    if algorithm_version.trim().is_empty() || parameter_version.trim().is_empty() {
        return Err(StructureError::InvalidParameters);
    }
    if candles.windows(2).any(|window| {
        window[0].open_time >= window[1].open_time
            || window[1].symbol != first.symbol
            || window[1].timeframe != first.timeframe
    }) {
        return Err(StructureError::InvalidCandleSequence);
    }
    Ok((&first.symbol, first.timeframe))
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StructureError {
    #[error("closed candle input must not be empty")]
    EmptyCandles,
    #[error("closed candles must be ordered and share a market key")]
    InvalidCandleSequence,
    #[error("structure parameters and versions must be positive/non-blank")]
    InvalidParameters,
    #[error("fact ordinal exceeded u32")]
    OrdinalOverflow,
}
