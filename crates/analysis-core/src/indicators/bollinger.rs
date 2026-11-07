use super::IndicatorError;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BollingerFact {
    pub period: usize,
    pub deviation_multiplier: Decimal,
    pub upper: Decimal,
    pub middle: Decimal,
    pub lower: Decimal,
    pub deviation: Decimal,
    pub bandwidth: Decimal,
    pub zscore: Decimal,
}

pub fn calculate_bollinger(
    closes: &[Decimal],
    period: usize,
    deviation_multiplier: Decimal,
) -> Result<Vec<Option<BollingerFact>>, IndicatorError> {
    if period == 0 || deviation_multiplier <= Decimal::ZERO {
        return Err(IndicatorError::InvalidPeriod);
    }
    let mut output = vec![None; closes.len()];
    if closes.len() < period {
        return Ok(output);
    }
    let divisor = Decimal::from(u32::try_from(period).map_err(|_| IndicatorError::InvalidPeriod)?);
    for index in period - 1..closes.len() {
        let window = &closes[index + 1 - period..=index];
        output[index] = Some(
            bollinger_fact(window, closes[index], divisor, period, deviation_multiplier)
                .ok_or(IndicatorError::ArithmeticOverflow)?,
        );
    }
    Ok(output)
}

/// Checked arithmetic: extreme prices yield `None` instead of a Decimal panic.
fn bollinger_fact(
    window: &[Decimal],
    close: Decimal,
    divisor: Decimal,
    period: usize,
    deviation_multiplier: Decimal,
) -> Option<BollingerFact> {
    let middle = window
        .iter()
        .try_fold(Decimal::ZERO, |sum, value| sum.checked_add(*value))?
        .checked_div(divisor)?;
    let variance = window
        .iter()
        .try_fold(Decimal::ZERO, |sum, value| {
            let difference = value.checked_sub(middle)?;
            sum.checked_add(difference.checked_mul(difference)?)
        })?
        .checked_div(divisor)?;
    let deviation = decimal_sqrt(variance);
    let band = deviation_multiplier.checked_mul(deviation)?;
    let upper = middle.checked_add(band)?;
    let lower = middle.checked_sub(band)?;
    let bandwidth = if middle == Decimal::ZERO {
        Decimal::ZERO
    } else {
        upper.checked_sub(lower)?.checked_div(middle.abs())?
    };
    let zscore = if deviation == Decimal::ZERO {
        Decimal::ZERO
    } else {
        close.checked_sub(middle)?.checked_div(deviation)?
    };
    Some(BollingerFact {
        period,
        deviation_multiplier,
        upper: upper.round_dp(6),
        middle: middle.round_dp(6),
        lower: lower.round_dp(6),
        deviation: deviation.round_dp(6),
        bandwidth: bandwidth.round_dp(6),
        zscore: zscore.round_dp(6),
    })
}

fn decimal_sqrt(value: Decimal) -> Decimal {
    if value <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut estimate = if value > Decimal::ONE {
        value / Decimal::TWO
    } else {
        Decimal::ONE
    };
    for _ in 0..32 {
        estimate = (estimate + value / estimate) / Decimal::TWO;
    }
    estimate
}
