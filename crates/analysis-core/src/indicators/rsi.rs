use super::{DivergenceKind, IndicatorError, IndicatorOutput, RegularDivergence, RsiPivot};
use crate::{ConfirmedSwing, SwingKind};
use analysis_domain::{FactId, FactKind, Readiness, ReadinessReason};
use rust_decimal::Decimal;
use std::collections::HashMap;

pub fn calculate_rsi(closes: &[Decimal], period: usize) -> Result<IndicatorOutput, IndicatorError> {
    if period == 0 {
        return Err(IndicatorError::InvalidPeriod);
    }
    let mut values = vec![None; closes.len()];
    if closes.len() <= period {
        return Ok(IndicatorOutput {
            readiness: Readiness::NotReady {
                reason: ReadinessReason::InsufficientHistory,
            },
            values,
        });
    }
    let divisor = Decimal::from(u32::try_from(period).map_err(|_| IndicatorError::InvalidPeriod)?);
    let mut gains = Decimal::ZERO;
    let mut losses = Decimal::ZERO;
    for index in 1..=period {
        let change = closes[index] - closes[index - 1];
        if change > Decimal::ZERO {
            gains += change;
        } else {
            losses += change.abs();
        }
    }
    let mut average_gain = gains / divisor;
    let mut average_loss = losses / divisor;
    values[period] = Some(rsi_value(average_gain, average_loss));
    let prior_weight =
        Decimal::from(u64::try_from(period - 1).map_err(|_| IndicatorError::InvalidPeriod)?);
    for index in period + 1..closes.len() {
        let change = closes[index] - closes[index - 1];
        let gain = change.max(Decimal::ZERO);
        let loss = (-change).max(Decimal::ZERO);
        average_gain = (average_gain * prior_weight + gain) / divisor;
        average_loss = (average_loss * prior_weight + loss) / divisor;
        values[index] = Some(rsi_value(average_gain, average_loss));
    }
    Ok(IndicatorOutput {
        readiness: Readiness::Ready,
        values,
    })
}

fn rsi_value(average_gain: Decimal, average_loss: Decimal) -> Decimal {
    if average_gain == Decimal::ZERO && average_loss == Decimal::ZERO {
        return Decimal::new(50, 0);
    }
    if average_loss == Decimal::ZERO {
        return Decimal::ONE_HUNDRED;
    }
    (Decimal::ONE_HUNDRED - Decimal::ONE_HUNDRED / (Decimal::ONE + average_gain / average_loss))
        .round_dp(6)
}

pub fn detect_regular_divergence(
    price_swings: &[ConfirmedSwing],
    rsi_pivots: &[RsiPivot],
    minimum_distance_bars: usize,
    maximum_distance_bars: usize,
    algorithm_version: &str,
    parameter_version: &str,
) -> Result<Vec<RegularDivergence>, IndicatorError> {
    if minimum_distance_bars == 0
        || maximum_distance_bars < minimum_distance_bars
        || algorithm_version.trim().is_empty()
        || parameter_version.trim().is_empty()
    {
        return Err(IndicatorError::InvalidPeriod);
    }
    let rsi_by_key: HashMap<_, _> = rsi_pivots
        .iter()
        .map(|pivot| ((pivot.kind, pivot.pivot_open_time), pivot))
        .collect();
    let mut divergences = Vec::new();
    for kind in [SwingKind::Low, SwingKind::High] {
        let matching: Vec<_> = price_swings
            .iter()
            .filter(|swing| swing.kind == kind)
            .collect();
        for pair in matching.windows(2) {
            let first_price = pair[0];
            let second_price = pair[1];
            let Some(first_rsi) = rsi_by_key.get(&(kind, first_price.pivot_open_time)) else {
                continue;
            };
            let Some(second_rsi) = rsi_by_key.get(&(kind, second_price.pivot_open_time)) else {
                continue;
            };
            validate_pivot_identity(first_price, first_rsi)?;
            validate_pivot_identity(second_price, second_rsi)?;
            let duration = first_price.id.timeframe.duration_ms();
            let distance = usize::try_from(
                (second_price.pivot_open_time - first_price.pivot_open_time) / duration,
            )
            .map_err(|_| IndicatorError::InvalidPivotIdentity)?;
            if !(minimum_distance_bars..=maximum_distance_bars).contains(&distance) {
                continue;
            }
            let divergence_kind = match kind {
                SwingKind::Low
                    if second_price.price < first_price.price
                        && second_rsi.value > first_rsi.value =>
                {
                    Some(DivergenceKind::RegularBullish)
                }
                SwingKind::High
                    if second_price.price > first_price.price
                        && second_rsi.value < first_rsi.value =>
                {
                    Some(DivergenceKind::RegularBearish)
                }
                _ => None,
            };
            let Some(divergence_kind) = divergence_kind else {
                continue;
            };
            divergences.push(RegularDivergence {
                id: FactId {
                    symbol: first_price.id.symbol.clone(),
                    timeframe: first_price.id.timeframe,
                    kind: FactKind::RsiDivergence,
                    confirmed_at: second_price.confirmed_at.max(second_rsi.confirmed_at),
                    ordinal: u32::try_from(divergences.len())
                        .map_err(|_| IndicatorError::OrdinalOverflow)?,
                    algorithm_version: algorithm_version.to_owned(),
                    parameter_version: parameter_version.to_owned(),
                },
                kind: divergence_kind,
                first_price_swing_id: first_price.id.clone(),
                second_price_swing_id: second_price.id.clone(),
                first_rsi_pivot_id: first_rsi.id.clone(),
                second_rsi_pivot_id: second_rsi.id.clone(),
            });
        }
    }
    divergences.sort_by_key(|fact| (fact.id.confirmed_at, fact.id.ordinal));
    Ok(divergences)
}

fn validate_pivot_identity(price: &ConfirmedSwing, rsi: &RsiPivot) -> Result<(), IndicatorError> {
    if price.id.symbol != rsi.id.symbol
        || price.id.timeframe != rsi.id.timeframe
        || price.kind != rsi.kind
        || price.pivot_open_time != rsi.pivot_open_time
        || price.confirmed_at > rsi.confirmed_at
    {
        return Err(IndicatorError::InvalidPivotIdentity);
    }
    Ok(())
}
