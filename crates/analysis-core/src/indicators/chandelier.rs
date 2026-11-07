use super::IndicatorError;
use market_domain::Candle;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChandelierFact {
    pub period: usize,
    pub atr_multiplier: Decimal,
    pub long_candidate: Decimal,
    pub short_candidate: Decimal,
    pub source_end_time: i64,
    pub effective_at: i64,
}

pub fn calculate_chandelier(
    candles: &[Candle],
    atr_values: &[Option<Decimal>],
    period: usize,
    atr_multiplier: Decimal,
) -> Result<Vec<Option<ChandelierFact>>, IndicatorError> {
    if period == 0 || atr_multiplier <= Decimal::ZERO || candles.len() != atr_values.len() {
        return Err(IndicatorError::InvalidPeriod);
    }
    if candles.windows(2).any(|window| {
        window[0].open_time >= window[1].open_time
            || window[0].symbol != window[1].symbol
            || window[0].timeframe != window[1].timeframe
    }) {
        return Err(IndicatorError::InvalidCandleSequence);
    }
    let mut output = vec![None; candles.len()];
    for index in period - 1..candles.len() {
        let Some(atr) = atr_values[index] else {
            continue;
        };
        let window = &candles[index + 1 - period..=index];
        let highest = window
            .iter()
            .map(|candle| candle.high)
            .max()
            .expect("period is positive");
        let lowest = window
            .iter()
            .map(|candle| candle.low)
            .min()
            .expect("period is positive");
        output[index] = Some(ChandelierFact {
            period,
            atr_multiplier,
            long_candidate: (highest - atr * atr_multiplier).round_dp(6),
            short_candidate: (lowest + atr * atr_multiplier).round_dp(6),
            source_end_time: candles[index].close_time,
            effective_at: candles[index]
                .close_time
                .checked_add(1)
                .ok_or(IndicatorError::InvalidCandleSequence)?,
        });
    }
    Ok(output)
}

pub fn next_long_stop(previous: Decimal, candidate: Decimal) -> Decimal {
    previous.max(candidate)
}

pub fn next_short_stop(previous: Decimal, candidate: Decimal) -> Decimal {
    previous.min(candidate)
}
