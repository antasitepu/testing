use super::{IndicatorError, IndicatorOutput};
use analysis_domain::{Readiness, ReadinessReason};
use market_domain::Candle;
use rust_decimal::Decimal;

pub fn calculate_adx(candles: &[Candle], period: usize) -> Result<IndicatorOutput, IndicatorError> {
    if period == 0 {
        return Err(IndicatorError::InvalidPeriod);
    }
    if candles.windows(2).any(|window| {
        window[0].open_time >= window[1].open_time
            || window[0].symbol != window[1].symbol
            || window[0].timeframe != window[1].timeframe
    }) {
        return Err(IndicatorError::InvalidCandleSequence);
    }
    let mut values = vec![None; candles.len()];
    let required = period
        .checked_mul(2)
        .and_then(|value| value.checked_sub(1))
        .ok_or(IndicatorError::InvalidPeriod)?;
    if candles.len() < required {
        return Ok(IndicatorOutput {
            readiness: Readiness::NotReady {
                reason: ReadinessReason::InsufficientHistory,
            },
            values,
        });
    }
    let mut true_ranges = Vec::with_capacity(candles.len());
    let mut plus_dm = Vec::with_capacity(candles.len());
    let mut minus_dm = Vec::with_capacity(candles.len());
    for (index, candle) in candles.iter().enumerate() {
        if index == 0 {
            true_ranges.push(candle.high - candle.low);
            plus_dm.push(Decimal::ZERO);
            minus_dm.push(Decimal::ZERO);
            continue;
        }
        let previous = &candles[index - 1];
        true_ranges.push(
            (candle.high - candle.low)
                .max((candle.high - previous.close).abs())
                .max((candle.low - previous.close).abs()),
        );
        let up_move = candle.high - previous.high;
        let down_move = previous.low - candle.low;
        plus_dm.push(if up_move > down_move && up_move > Decimal::ZERO {
            up_move
        } else {
            Decimal::ZERO
        });
        minus_dm.push(if down_move > up_move && down_move > Decimal::ZERO {
            down_move
        } else {
            Decimal::ZERO
        });
    }
    let divisor = Decimal::from(u32::try_from(period).map_err(|_| IndicatorError::InvalidPeriod)?);
    let mut smoothed_tr = true_ranges[..period].iter().copied().sum::<Decimal>();
    let mut smoothed_plus = plus_dm[..period].iter().copied().sum::<Decimal>();
    let mut smoothed_minus = minus_dm[..period].iter().copied().sum::<Decimal>();
    let mut dx_values = Vec::new();
    let mut adx = None;
    for index in period - 1..candles.len() {
        if index >= period {
            smoothed_tr = smoothed_tr - smoothed_tr / divisor + true_ranges[index];
            smoothed_plus = smoothed_plus - smoothed_plus / divisor + plus_dm[index];
            smoothed_minus = smoothed_minus - smoothed_minus / divisor + minus_dm[index];
        }
        let plus_di = if smoothed_tr == Decimal::ZERO {
            Decimal::ZERO
        } else {
            Decimal::ONE_HUNDRED * smoothed_plus / smoothed_tr
        };
        let minus_di = if smoothed_tr == Decimal::ZERO {
            Decimal::ZERO
        } else {
            Decimal::ONE_HUNDRED * smoothed_minus / smoothed_tr
        };
        let sum = plus_di + minus_di;
        let dx = if sum == Decimal::ZERO {
            Decimal::ZERO
        } else {
            Decimal::ONE_HUNDRED * (plus_di - minus_di).abs() / sum
        };
        dx_values.push(dx);
        if dx_values.len() == period {
            let seed = dx_values.iter().copied().sum::<Decimal>() / divisor;
            adx = Some(seed);
            values[index] = Some(seed.round_dp(6));
        } else if let Some(previous_adx) = adx {
            let next = (previous_adx * Decimal::from((period - 1) as u64) + dx) / divisor;
            adx = Some(next);
            values[index] = Some(next.round_dp(6));
        }
    }
    Ok(IndicatorOutput {
        readiness: Readiness::Ready,
        values,
    })
}
