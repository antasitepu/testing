use super::IndicatorError;
use analysis_domain::{Readiness, ReadinessReason};
use rust_decimal::Decimal;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmaOutput {
    pub readiness: Readiness,
    pub values: Vec<Option<Decimal>>,
    pub slopes: Vec<Option<Decimal>>,
}

pub fn calculate_ema(
    closes: &[Decimal],
    period: usize,
    slope_lookback: usize,
) -> Result<EmaOutput, IndicatorError> {
    if period == 0 || slope_lookback == 0 {
        return Err(IndicatorError::InvalidPeriod);
    }
    let mut values = vec![None; closes.len()];
    let mut slopes = vec![None; closes.len()];
    if closes.len() < period {
        return Ok(EmaOutput {
            readiness: Readiness::NotReady {
                reason: ReadinessReason::InsufficientHistory,
            },
            values,
            slopes,
        });
    }
    let period_decimal =
        Decimal::from(u32::try_from(period).map_err(|_| IndicatorError::InvalidPeriod)?);
    let multiplier = Decimal::TWO / (period_decimal + Decimal::ONE);
    let seed = closes[..period].iter().copied().sum::<Decimal>() / period_decimal;
    values[period - 1] = Some(seed.round_dp(6));
    let mut ema = seed;
    for index in period..closes.len() {
        ema = (closes[index] - ema) * multiplier + ema;
        values[index] = Some(ema.round_dp(6));
    }
    for index in period - 1 + slope_lookback..closes.len() {
        if let (Some(current), Some(previous)) = (values[index], values[index - slope_lookback]) {
            slopes[index] = Some((current - previous).round_dp(6));
        }
    }
    Ok(EmaOutput {
        readiness: Readiness::Ready,
        values,
        slopes,
    })
}
