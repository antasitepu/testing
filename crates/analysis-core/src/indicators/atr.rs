use super::{IndicatorError, IndicatorOutput};
use analysis_domain::{Readiness, ReadinessReason};
use market_domain::Candle;
use rust_decimal::Decimal;

pub fn calculate_atr(candles: &[Candle], period: usize) -> Result<IndicatorOutput, IndicatorError> {
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
    if candles.len() < period {
        return Ok(IndicatorOutput {
            readiness: Readiness::NotReady {
                reason: ReadinessReason::InsufficientHistory,
            },
            values,
        });
    }
    let true_ranges: Vec<_> = candles
        .iter()
        .enumerate()
        .map(|(index, candle)| {
            let range = candle.high - candle.low;
            if index == 0 {
                range
            } else {
                let previous_close = candles[index - 1].close;
                range
                    .max((candle.high - previous_close).abs())
                    .max((candle.low - previous_close).abs())
            }
        })
        .collect();
    let divisor = Decimal::from(u32::try_from(period).map_err(|_| IndicatorError::InvalidPeriod)?);
    let mut atr = true_ranges[..period].iter().copied().sum::<Decimal>() / divisor;
    values[period - 1] = Some(atr.round_dp(6));
    for index in period..candles.len() {
        atr = ((atr * Decimal::from((period - 1) as u64)) + true_ranges[index]) / divisor;
        values[index] = Some(atr.round_dp(6));
    }
    Ok(IndicatorOutput {
        readiness: Readiness::Ready,
        values,
    })
}
