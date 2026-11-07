use crate::PlanKind;
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReleaseMode {
    Research,
    PaperActive,
}

impl ReleaseMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Research => "RESEARCH",
            Self::PaperActive => "PAPER_ACTIVE",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionIdentity {
    pub release_id: Uuid,
    pub algorithm_version: String,
    pub parameter_version: String,
}

impl VersionIdentity {
    pub fn new(
        release_id: Uuid,
        algorithm_version: impl Into<String>,
        parameter_version: impl Into<String>,
    ) -> Result<Self, VersionIdentityError> {
        let algorithm_version = algorithm_version.into();
        let parameter_version = parameter_version.into();
        if algorithm_version.trim().is_empty() {
            return Err(VersionIdentityError::BlankAlgorithmVersion);
        }
        if parameter_version.trim().is_empty() {
            return Err(VersionIdentityError::BlankParameterVersion);
        }
        Ok(Self {
            release_id,
            algorithm_version,
            parameter_version,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum VersionIdentityError {
    #[error("algorithm version must not be blank")]
    BlankAlgorithmVersion,
    #[error("parameter version must not be blank")]
    BlankParameterVersion,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProfileKey {
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub target_timeframe: Timeframe,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HtfFilterMode {
    Strict,
    Weighted,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IctFvgSelection {
    #[default]
    RelatedDisplacement,
    NearestAfterDisplacement,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IctSweepBiasMode {
    #[default]
    SameBias,
    OppositeBias,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IctParameters {
    pub higher_timeframe: Option<Timeframe>,
    #[serde(default)]
    pub fvg_selection: IctFvgSelection,
    #[serde(default)]
    pub sweep_bias_mode: IctSweepBiasMode,
    pub atr_period: i32,
    pub atr_buffer_multiplier: Decimal,
    pub minimum_rr: Decimal,
    pub bias_max_bars_valid: i32,
    pub sweep_max_bars_valid: i32,
    pub displacement_max_bars_valid: i32,
    pub entry_max_bars_valid: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmcParameters {
    pub higher_timeframe: Option<Timeframe>,
    pub htf_mode: HtfFilterMode,
    pub weighted_countertrend_penalty: Decimal,
    pub atr_period: i32,
    pub atr_buffer_multiplier: Decimal,
    pub minimum_rr: Decimal,
    pub max_bars_valid: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RsiPlanParameters {
    pub rsi_period: i32,
    pub oversold_threshold: Decimal,
    pub overbought_threshold: Decimal,
    pub divergence_required: bool,
    pub minimum_pivot_distance: i32,
    pub maximum_pivot_distance: i32,
    pub confirmed_swing_lookback: i32,
    pub atr_period: i32,
    pub atr_buffer_multiplier: Decimal,
    pub target_rr: Decimal,
    pub max_bars_valid: i32,
    #[serde(
        default = "default_rsi_reversal_confirmation_bars",
        skip_serializing_if = "is_default_rsi_reversal_confirmation_bars"
    )]
    pub reversal_confirmation_bars: i32,
}

pub const fn default_rsi_reversal_confirmation_bars() -> i32 {
    2
}

fn is_default_rsi_reversal_confirmation_bars(value: &i32) -> bool {
    *value == default_rsi_reversal_confirmation_bars()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuasimodoParameters {
    pub minimum_zone_ticks: i32,
    pub tolerance_atr_multiplier: Decimal,
    pub atr_period: i32,
    pub atr_buffer_multiplier: Decimal,
    pub minimum_rr: Decimal,
    pub max_bars_valid: i32,
    #[serde(default = "default_qm_max_structure_age_bars")]
    pub max_structure_age_bars: i32,
    #[serde(default = "default_qm_max_entry_distance_atr")]
    pub max_entry_distance_atr: Decimal,
}

pub const fn default_qm_max_structure_age_bars() -> i32 {
    10
}

pub fn default_qm_max_entry_distance_atr() -> Decimal {
    Decimal::new(5, 0)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendBreakoutParameters {
    pub donchian_period: i32,
    pub ema_period: i32,
    pub adx_period: i32,
    pub minimum_adx: Decimal,
    pub higher_timeframe: Option<Timeframe>,
    pub atr_period: i32,
    pub initial_stop_atr_multiplier: Decimal,
    pub target_rr: Decimal,
    pub trailing_activation_r: Decimal,
    pub chandelier_period: i32,
    pub chandelier_atr_multiplier: Decimal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanReversionParameters {
    pub bollinger_period: i32,
    pub bollinger_deviation: Decimal,
    pub zscore_period: i32,
    pub extreme_zscore: Decimal,
    pub adx_period: i32,
    pub maximum_adx: Decimal,
    pub maximum_bandwidth: Decimal,
    pub atr_period: i32,
    pub atr_buffer_multiplier: Decimal,
    pub minimum_rr: Decimal,
    pub max_bars_valid: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProfitProtectionParameters {
    pub aligned_activation_r: Decimal,
    pub counter_activation_r: Decimal,
    pub lock_step_r: Decimal,
    pub reversal_giveback_r: Decimal,
    pub adverse_closes: u32,
}

impl Default for ProfitProtectionParameters {
    fn default() -> Self {
        Self {
            aligned_activation_r: Decimal::ONE,
            counter_activation_r: Decimal::new(5, 1),
            lock_step_r: Decimal::new(5, 1),
            reversal_giveback_r: Decimal::new(5, 1),
            adverse_closes: 2,
        }
    }
}

impl ProfitProtectionParameters {
    pub fn validate(&self) -> Result<(), PlanParameterError> {
        positive_decimals(&[
            self.aligned_activation_r,
            self.counter_activation_r,
            self.lock_step_r,
            self.reversal_giveback_r,
        ])?;
        if self.adverse_closes == 0 {
            return Err(PlanParameterError::NonPositiveConfirmationCount);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PositionManagementParameters {
    pub zone_buffer_multiplier: Decimal,
    pub minimum_buffer_ticks: u32,
    pub target_buffer_atr_multiplier: Decimal,
    pub target_buffer_minimum_ticks: u32,
    pub tp1_r: Decimal,
    pub tp1_fraction: Decimal,
    pub weakening_giveback_r: Decimal,
}

impl Default for PositionManagementParameters {
    fn default() -> Self {
        Self {
            zone_buffer_multiplier: Decimal::new(5, 1),
            minimum_buffer_ticks: 10,
            target_buffer_atr_multiplier: Decimal::new(1, 1),
            target_buffer_minimum_ticks: 5,
            tp1_r: Decimal::ONE,
            tp1_fraction: Decimal::new(5, 1),
            weakening_giveback_r: Decimal::new(25, 2),
        }
    }
}

impl PositionManagementParameters {
    pub fn validate(&self) -> Result<(), PlanParameterError> {
        positive_decimals(&[
            self.zone_buffer_multiplier,
            self.target_buffer_atr_multiplier,
            self.tp1_r,
            self.tp1_fraction,
            self.weakening_giveback_r,
        ])?;
        if self.minimum_buffer_ticks == 0 || self.target_buffer_minimum_ticks == 0 {
            return Err(PlanParameterError::NonPositiveTickCount);
        }
        if self.tp1_fraction >= Decimal::ONE {
            return Err(PlanParameterError::InvalidFraction);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterializedProfile {
    pub key: PlanProfileKey,
    pub enabled: bool,
    pub versions: VersionIdentity,
    pub parameters: PlanParameters,
    #[serde(default)]
    pub profit_protection: ProfitProtectionParameters,
    #[serde(default)]
    pub position_management: PositionManagementParameters,
    pub required_timeframes: Vec<Timeframe>,
    pub minimum_history_bars: usize,
    pub warmup_bars: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "parameters",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum PlanParameters {
    Ict(IctParameters),
    Smc(SmcParameters),
    Rsi(RsiPlanParameters),
    Quasimodo(QuasimodoParameters),
    TrendBreakout(TrendBreakoutParameters),
    MeanReversion(MeanReversionParameters),
}

impl PlanParameters {
    pub const fn plan(&self) -> PlanKind {
        match self {
            Self::Ict(_) => PlanKind::Ict,
            Self::Smc(_) => PlanKind::Smc,
            Self::Rsi(_) => PlanKind::Rsi,
            Self::Quasimodo(_) => PlanKind::Quasimodo,
            Self::TrendBreakout(_) => PlanKind::TrendBreakout,
            Self::MeanReversion(_) => PlanKind::MeanReversion,
        }
    }

    pub fn higher_timeframe(&self) -> Option<Timeframe> {
        match self {
            Self::Ict(parameters) => parameters.higher_timeframe,
            Self::Smc(parameters) => parameters.higher_timeframe,
            Self::TrendBreakout(parameters) => parameters.higher_timeframe,
            Self::Rsi(_) | Self::Quasimodo(_) | Self::MeanReversion(_) => None,
        }
    }

    pub fn validate(&self) -> Result<(), PlanParameterError> {
        match self {
            Self::Ict(parameters) => {
                positive_periods(&[
                    parameters.atr_period,
                    parameters.bias_max_bars_valid,
                    parameters.sweep_max_bars_valid,
                    parameters.displacement_max_bars_valid,
                    parameters.entry_max_bars_valid,
                ])?;
                positive_decimals(&[parameters.atr_buffer_multiplier, parameters.minimum_rr])
            }
            Self::Smc(parameters) => {
                positive_periods(&[parameters.atr_period, parameters.max_bars_valid])?;
                positive_decimals(&[
                    parameters.weighted_countertrend_penalty,
                    parameters.atr_buffer_multiplier,
                    parameters.minimum_rr,
                ])
            }
            Self::Rsi(parameters) => {
                positive_periods(&[
                    parameters.rsi_period,
                    parameters.minimum_pivot_distance,
                    parameters.maximum_pivot_distance,
                    parameters.confirmed_swing_lookback,
                    parameters.atr_period,
                    parameters.max_bars_valid,
                    parameters.reversal_confirmation_bars,
                ])?;
                if parameters.minimum_pivot_distance > parameters.maximum_pivot_distance {
                    return Err(PlanParameterError::InvalidRange);
                }
                if parameters.oversold_threshold <= Decimal::ZERO
                    || parameters.oversold_threshold >= parameters.overbought_threshold
                    || parameters.overbought_threshold >= Decimal::ONE_HUNDRED
                {
                    return Err(PlanParameterError::InvalidRsiThresholds);
                }
                positive_decimals(&[parameters.atr_buffer_multiplier, parameters.target_rr])
            }
            Self::Quasimodo(parameters) => {
                positive_periods(&[
                    parameters.minimum_zone_ticks,
                    parameters.atr_period,
                    parameters.max_bars_valid,
                    parameters.max_structure_age_bars,
                ])?;
                positive_decimals(&[
                    parameters.tolerance_atr_multiplier,
                    parameters.atr_buffer_multiplier,
                    parameters.minimum_rr,
                    parameters.max_entry_distance_atr,
                ])
            }
            Self::TrendBreakout(parameters) => {
                positive_periods(&[
                    parameters.donchian_period,
                    parameters.ema_period,
                    parameters.adx_period,
                    parameters.atr_period,
                    parameters.chandelier_period,
                ])?;
                positive_decimals(&[
                    parameters.minimum_adx,
                    parameters.initial_stop_atr_multiplier,
                    parameters.target_rr,
                    parameters.trailing_activation_r,
                    parameters.chandelier_atr_multiplier,
                ])
            }
            Self::MeanReversion(parameters) => {
                positive_periods(&[
                    parameters.bollinger_period,
                    parameters.zscore_period,
                    parameters.adx_period,
                    parameters.atr_period,
                    parameters.max_bars_valid,
                ])?;
                positive_decimals(&[
                    parameters.bollinger_deviation,
                    parameters.extreme_zscore,
                    parameters.maximum_adx,
                    parameters.maximum_bandwidth,
                    parameters.atr_buffer_multiplier,
                    parameters.minimum_rr,
                ])
            }
        }
    }
}

fn positive_periods(values: &[i32]) -> Result<(), PlanParameterError> {
    if values.iter().any(|value| *value <= 0) {
        return Err(PlanParameterError::NonPositivePeriod);
    }
    Ok(())
}

fn positive_decimals(values: &[Decimal]) -> Result<(), PlanParameterError> {
    if values.iter().any(|value| *value <= Decimal::ZERO) {
        return Err(PlanParameterError::NonPositiveValue);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanParameterError {
    #[error("periods and lookbacks must be positive")]
    NonPositivePeriod,
    #[error("parameter values must be positive")]
    NonPositiveValue,
    #[error("confirmation counts must be positive")]
    NonPositiveConfirmationCount,
    #[error("tick counts must be positive")]
    NonPositiveTickCount,
    #[error("fractions must be greater than zero and less than one")]
    InvalidFraction,
    #[error("parameter range is inverted")]
    InvalidRange,
    #[error("RSI thresholds must satisfy 0 < oversold < overbought < 100")]
    InvalidRsiThresholds,
}
