use crate::{ConfirmedSwing, SwingKind};
use analysis_domain::{Direction, FactId, FactKind, FactLifecycle};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FairValueGapDirection {
    Bullish,
    Bearish,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FairValueGap {
    pub id: FactId,
    pub direction: FairValueGapDirection,
    pub low: Decimal,
    pub high: Decimal,
    pub formation_open_times: [i64; 3],
    pub related_displacement_id: Option<FactId>,
    pub lifecycle: FactLifecycle,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LiquiditySide {
    BuySide,
    SellSide,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquidityPool {
    pub id: FactId,
    pub side: LiquiditySide,
    pub level: Decimal,
    pub lower_bound: Decimal,
    pub upper_bound: Decimal,
    pub source_swing_ids: Vec<FactId>,
    pub lifecycle: FactLifecycle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquiditySweep {
    pub id: FactId,
    pub direction: Direction,
    pub source_pool_id: FactId,
    pub source_swing_ids: Vec<FactId>,
    pub sweep_candle_open_time: i64,
    pub extreme: Decimal,
}

pub fn detect_fair_value_gaps(
    candles: &[Candle],
    minimum_size: Decimal,
    related_displacement_id: Option<FactId>,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<FairValueGap>, LiquidityError> {
    let (symbol, timeframe) = validate_candles(candles)?;
    validate_parameters(minimum_size, algorithm_version, parameter_version)?;
    if let Some(displacement_id) = &related_displacement_id
        && (&displacement_id.symbol != symbol
            || displacement_id.timeframe != timeframe
            || displacement_id.kind != FactKind::Displacement)
    {
        return Err(LiquidityError::InvalidRelationship);
    }

    let mut gaps = Vec::new();
    for index in 0..candles.len().saturating_sub(2) {
        let first = &candles[index];
        let third = &candles[index + 2];
        let candidate = if third.low - first.high >= minimum_size {
            Some((FairValueGapDirection::Bullish, first.high, third.low))
        } else if first.low - third.high >= minimum_size {
            Some((FairValueGapDirection::Bearish, third.high, first.low))
        } else {
            None
        };
        let Some((direction, low, high)) = candidate else {
            continue;
        };
        let lifecycle = fvg_lifecycle(direction, low, high, &candles[index + 3..]);
        gaps.push(FairValueGap {
            id: FactId {
                symbol: symbol.clone(),
                timeframe,
                kind: FactKind::FairValueGap,
                confirmed_at: third.close_time,
                ordinal: u32::try_from(gaps.len()).map_err(|_| LiquidityError::OrdinalOverflow)?,
                algorithm_version: algorithm_version.to_owned(),
                parameter_version: parameter_version.to_owned(),
            },
            direction,
            low,
            high,
            formation_open_times: [
                first.open_time,
                candles[index + 1].open_time,
                third.open_time,
            ],
            related_displacement_id: related_displacement_id.clone(),
            lifecycle,
        });
    }
    Ok(gaps)
}

pub fn fvg_lifecycle(
    direction: FairValueGapDirection,
    low: Decimal,
    high: Decimal,
    future: &[Candle],
) -> FactLifecycle {
    let mut lifecycle = FactLifecycle::Confirmed;
    for candle in future {
        match direction {
            FairValueGapDirection::Bullish if candle.low <= low => {
                return FactLifecycle::Filled;
            }
            FairValueGapDirection::Bullish if candle.low < high => {
                lifecycle = FactLifecycle::PartiallyFilled;
            }
            FairValueGapDirection::Bearish if candle.high >= high => {
                return FactLifecycle::Filled;
            }
            FairValueGapDirection::Bearish if candle.high > low => {
                lifecycle = FactLifecycle::PartiallyFilled;
            }
            _ => {}
        }
    }
    lifecycle
}

pub fn detect_liquidity_pools(
    swings: &[ConfirmedSwing],
    tolerance: Decimal,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<LiquidityPool>, LiquidityError> {
    if swings.is_empty() {
        return Ok(Vec::new());
    }
    validate_parameters(tolerance, algorithm_version, parameter_version)?;
    let symbol = &swings[0].id.symbol;
    let timeframe = swings[0].id.timeframe;
    if swings
        .iter()
        .any(|swing| &swing.id.symbol != symbol || swing.id.timeframe != timeframe)
    {
        return Err(LiquidityError::MixedMarketKey);
    }
    let mut pools = Vec::new();
    for (kind, side) in [
        (SwingKind::High, LiquiditySide::BuySide),
        (SwingKind::Low, LiquiditySide::SellSide),
    ] {
        let mut matching: Vec<_> = swings.iter().filter(|swing| swing.kind == kind).collect();
        matching.sort_by_key(|swing| (swing.pivot_open_time, swing.id.ordinal));
        let mut start = 0;
        while start < matching.len() {
            let mut end = start + 1;
            let mut lower = matching[start].price;
            let mut upper = matching[start].price;
            while end < matching.len() {
                let next_lower = lower.min(matching[end].price);
                let next_upper = upper.max(matching[end].price);
                if next_upper - next_lower > tolerance {
                    break;
                }
                lower = next_lower;
                upper = next_upper;
                end += 1;
            }
            if end - start >= 2 {
                let group = &matching[start..end];
                let total = group
                    .iter()
                    .fold(Decimal::ZERO, |sum, swing| sum + swing.price);
                let divisor = Decimal::from(
                    u32::try_from(group.len()).map_err(|_| LiquidityError::OrdinalOverflow)?,
                );
                let confirmed_at = group
                    .iter()
                    .map(|swing| swing.confirmed_at)
                    .max()
                    .expect("group is non-empty");
                pools.push(LiquidityPool {
                    id: FactId {
                        symbol: symbol.clone(),
                        timeframe,
                        kind: FactKind::Liquidity,
                        confirmed_at,
                        ordinal: u32::try_from(pools.len())
                            .map_err(|_| LiquidityError::OrdinalOverflow)?,
                        algorithm_version: algorithm_version.to_owned(),
                        parameter_version: parameter_version.to_owned(),
                    },
                    side,
                    level: total / divisor,
                    lower_bound: lower,
                    upper_bound: upper,
                    source_swing_ids: group.iter().map(|swing| swing.id.clone()).collect(),
                    lifecycle: FactLifecycle::Confirmed,
                });
            }
            start = end;
        }
    }
    pools.sort_by_key(|pool| (pool.id.confirmed_at, pool.id.ordinal));
    Ok(pools)
}

pub fn detect_liquidity_sweeps(
    pools: &[LiquidityPool],
    candles: &[Candle],
    tolerance: Decimal,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<LiquiditySweep>, LiquidityError> {
    validate_parameters(tolerance, algorithm_version, parameter_version)?;
    if pools.is_empty() || candles.is_empty() {
        return Ok(Vec::new());
    }
    let (symbol, timeframe) = validate_candles(candles)?;
    let mut swept = HashSet::new();
    let mut sweeps = Vec::new();
    for candle in candles {
        for pool in pools {
            if swept.contains(&pool.id)
                || &pool.id.symbol != symbol
                || pool.id.timeframe != timeframe
                || candle.close_time < pool.id.confirmed_at
            {
                continue;
            }
            let result = match pool.side {
                LiquiditySide::BuySide
                    if candle.high > pool.upper_bound + tolerance && candle.close < pool.level =>
                {
                    Some((Direction::Sell, candle.high))
                }
                LiquiditySide::SellSide
                    if candle.low < pool.lower_bound - tolerance && candle.close > pool.level =>
                {
                    Some((Direction::Buy, candle.low))
                }
                _ => None,
            };
            let Some((direction, extreme)) = result else {
                continue;
            };
            swept.insert(pool.id.clone());
            sweeps.push(LiquiditySweep {
                id: FactId {
                    symbol: symbol.clone(),
                    timeframe,
                    kind: FactKind::LiquiditySweep,
                    confirmed_at: candle.close_time,
                    ordinal: u32::try_from(sweeps.len())
                        .map_err(|_| LiquidityError::OrdinalOverflow)?,
                    algorithm_version: algorithm_version.to_owned(),
                    parameter_version: parameter_version.to_owned(),
                },
                direction,
                source_pool_id: pool.id.clone(),
                source_swing_ids: pool.source_swing_ids.clone(),
                sweep_candle_open_time: candle.open_time,
                extreme,
            });
        }
    }
    Ok(sweeps)
}

pub fn related_sequence(
    fvg_displacement_id: Option<&FactId>,
    sweep_displacement_id: Option<&FactId>,
    expected_displacement_id: &FactId,
) -> bool {
    expected_displacement_id.kind == FactKind::Displacement
        && fvg_displacement_id == Some(expected_displacement_id)
        && sweep_displacement_id == Some(expected_displacement_id)
}

fn validate_parameters(
    value: Decimal,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<(), LiquidityError> {
    if value <= Decimal::ZERO
        || algorithm_version.trim().is_empty()
        || parameter_version.trim().is_empty()
    {
        return Err(LiquidityError::InvalidParameters);
    }
    Ok(())
}

fn validate_candles(candles: &[Candle]) -> Result<(&Symbol, Timeframe), LiquidityError> {
    let first = candles.first().ok_or(LiquidityError::EmptyCandles)?;
    if candles.windows(2).any(|window| {
        window[0].open_time >= window[1].open_time
            || window[1].symbol != first.symbol
            || window[1].timeframe != first.timeframe
    }) {
        return Err(LiquidityError::InvalidCandleSequence);
    }
    Ok((&first.symbol, first.timeframe))
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LiquidityError {
    #[error("closed candle input must not be empty")]
    EmptyCandles,
    #[error("closed candles must be ordered and share a market key")]
    InvalidCandleSequence,
    #[error("liquidity parameters and versions must be positive/non-blank")]
    InvalidParameters,
    #[error("facts do not share a market key")]
    MixedMarketKey,
    #[error("fact relationship is invalid")]
    InvalidRelationship,
    #[error("fact ordinal exceeded u32")]
    OrdinalOverflow,
}
