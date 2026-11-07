use super::IndicatorError;
use market_domain::Candle;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DonchianFact {
    pub period: usize,
    pub upper: Decimal,
    pub lower: Decimal,
    pub source_end_time: i64,
}

pub fn calculate_previous_donchian(
    candles: &[Candle],
    period: usize,
) -> Result<Vec<Option<DonchianFact>>, IndicatorError> {
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
    let mut output = vec![None; candles.len()];
    for index in period..candles.len() {
        let source = &candles[index - period..index];
        let upper = source
            .iter()
            .map(|candle| candle.high)
            .max()
            .expect("period is positive");
        let lower = source
            .iter()
            .map(|candle| candle.low)
            .min()
            .expect("period is positive");
        output[index] = Some(DonchianFact {
            period,
            upper,
            lower,
            source_end_time: source.last().expect("period is positive").close_time,
        });
    }
    Ok(output)
}
