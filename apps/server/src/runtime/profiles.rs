use analysis_domain::{
    HtfFilterMode, IctFvgSelection, IctParameters, IctSweepBiasMode, MeanReversionParameters,
    PlanKind, PlanParameters, PlanProfileKey, PositionManagementParameters,
    ProfitProtectionParameters, QuasimodoParameters, ReleaseMode, RsiPlanParameters,
    SessionBreakoutParameters, SmcParameters, TrendBreakoutParameters, VersionIdentity,
    VolatilitySqueezeParameters,
};
use analysis_plans::{
    IctEvaluator, MeanReversionEvaluator, PlanEvaluator, QuasimodoEvaluator, RsiEvaluator,
    SessionBreakoutEvaluator, SmcEvaluator, TrendBreakoutEvaluator, VolatilitySqueezeEvaluator,
};
use analysis_service::{
    AnalysisServiceError, ConfigurationRelease, MaterializedProfile, PlanRegistry,
    ReleaseLifecycle, ValidationGate,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};
use uuid::Uuid;

const EXPECTED_PROFILE_COUNT: usize = 56;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProfileCatalogEntry {
    pub plan: PlanKind,
    pub timeframe: Timeframe,
    pub enabled: bool,
    pub algorithm_version: String,
    pub parameter_version: String,
    pub parameters: PlanParameters,
    pub profit_protection: ProfitProtectionParameters,
    pub position_management: PositionManagementParameters,
    pub required_timeframes: Vec<Timeframe>,
    pub minimum_history_bars: usize,
    pub warmup_bars: usize,
}

impl RuntimeProfileCatalogEntry {
    fn materialize(
        &self,
        release_id: Uuid,
        symbol: Symbol,
    ) -> Result<MaterializedProfile, AnalysisServiceError> {
        Ok(MaterializedProfile {
            key: PlanProfileKey {
                symbol,
                plan: self.plan,
                target_timeframe: self.timeframe,
            },
            enabled: self.enabled,
            versions: VersionIdentity::new(
                release_id,
                self.algorithm_version.clone(),
                self.parameter_version.clone(),
            )
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
            parameters: self.parameters.clone(),
            profit_protection: self.profit_protection.clone(),
            position_management: self.position_management.clone(),
            required_timeframes: self.required_timeframes.clone(),
            minimum_history_bars: self.minimum_history_bars,
            warmup_bars: self.warmup_bars,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeProfileCatalog {
    entries: Vec<RuntimeProfileCatalogEntry>,
    fingerprint: String,
}

impl RuntimeProfileCatalog {
    pub fn current() -> Result<Self, AnalysisServiceError> {
        let mut entries = PlanKind::ALL
            .into_iter()
            .flat_map(|plan| {
                Timeframe::ALL
                    .into_iter()
                    .map(move |timeframe| catalog_entry(plan, timeframe))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| (entry.plan, entry.timeframe.duration_ms()));
        let fingerprint = serde_json::to_string(&entries).map_err(|error| {
            AnalysisServiceError::InvalidData(format!(
                "serialize runtime profile catalog fingerprint: {error}"
            ))
        })?;
        let catalog = Self {
            entries,
            fingerprint,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn entries(&self) -> &[RuntimeProfileCatalogEntry] {
        &self.entries
    }

    pub fn validate(&self) -> Result<(), AnalysisServiceError> {
        if self.entries.len() != EXPECTED_PROFILE_COUNT {
            return Err(AnalysisServiceError::InvalidData(format!(
                "runtime profile catalog must contain {EXPECTED_PROFILE_COUNT} entries, found {}",
                self.entries.len()
            )));
        }

        let registry = runtime_plan_registry();
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            if registry.get(entry.plan).is_none() {
                return Err(AnalysisServiceError::InvalidData(format!(
                    "runtime profile catalog has no evaluator for {}",
                    entry.plan.as_str()
                )));
            }
            if entry.parameters.plan() != entry.plan {
                return Err(AnalysisServiceError::InvalidData(format!(
                    "runtime profile catalog parameters do not match {} {}",
                    entry.plan.as_str(),
                    entry.timeframe.as_str()
                )));
            }
            if !keys.insert((entry.plan, entry.timeframe)) {
                return Err(AnalysisServiceError::InvalidData(format!(
                    "runtime profile catalog contains duplicate {} {}",
                    entry.plan.as_str(),
                    entry.timeframe.as_str()
                )));
            }
        }

        for plan in PlanKind::ALL {
            for timeframe in Timeframe::ALL {
                if !keys.contains(&(plan, timeframe)) {
                    return Err(AnalysisServiceError::InvalidData(format!(
                        "runtime profile catalog is missing {} {}",
                        plan.as_str(),
                        timeframe.as_str()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn profile(
        &self,
        release_id: Uuid,
        symbol: Symbol,
        plan: PlanKind,
        timeframe: Timeframe,
    ) -> Result<MaterializedProfile, AnalysisServiceError> {
        self.entries
            .iter()
            .find(|entry| entry.plan == plan && entry.timeframe == timeframe)
            .ok_or_else(|| {
                AnalysisServiceError::InvalidData(format!(
                    "runtime profile catalog is missing {} {}",
                    plan.as_str(),
                    timeframe.as_str()
                ))
            })?
            .materialize(release_id, symbol)
    }

    pub fn materialize_release(
        &self,
        symbol: Symbol,
    ) -> Result<ConfigurationRelease, AnalysisServiceError> {
        let release_id = Uuid::now_v7();
        let profiles = self
            .entries
            .iter()
            .map(|entry| entry.materialize(release_id, symbol.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ConfigurationRelease {
            release_id,
            symbol,
            mode: ReleaseMode::PaperActive,
            lifecycle: ReleaseLifecycle::Draft,
            validation_summary: json!({"source": "runtime_bootstrap"}),
            profiles,
        })
    }

    pub fn matches_release(
        &self,
        release_id: Uuid,
        symbol: &Symbol,
        profiles: &[MaterializedProfile],
    ) -> bool {
        if profiles.len() != self.entries.len() {
            return false;
        }
        self.entries.iter().all(|entry| {
            let Some(actual) = profiles.iter().find(|profile| {
                profile.key.symbol == *symbol
                    && profile.key.plan == entry.plan
                    && profile.key.target_timeframe == entry.timeframe
            }) else {
                return false;
            };
            let Ok(expected) = entry.materialize(release_id, symbol.clone()) else {
                return false;
            };
            profile_matches(actual, &expected)
        })
    }

    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }
}

pub(super) fn runtime_plan_registry() -> PlanRegistry {
    let evaluators: Vec<Arc<dyn PlanEvaluator>> = vec![
        Arc::new(IctEvaluator),
        Arc::new(SmcEvaluator),
        Arc::new(RsiEvaluator),
        Arc::new(QuasimodoEvaluator),
        Arc::new(TrendBreakoutEvaluator),
        Arc::new(MeanReversionEvaluator),
        Arc::new(SessionBreakoutEvaluator),
        Arc::new(VolatilitySqueezeEvaluator),
    ];
    PlanRegistry::new(evaluators)
}

#[cfg(test)]
pub(super) fn default_runtime_profile(
    release_id: Uuid,
    symbol: Symbol,
    plan: PlanKind,
    target_timeframe: Timeframe,
) -> Result<MaterializedProfile, AnalysisServiceError> {
    RuntimeProfileCatalog::current()?.profile(release_id, symbol, plan, target_timeframe)
}

pub(super) fn default_parameters(plan: PlanKind, timeframe: Timeframe) -> PlanParameters {
    let higher_timeframe = next_higher_timeframe(timeframe);
    match plan {
        PlanKind::Ict => {
            let (sweep_bias_mode, atr_buffer_multiplier, minimum_rr, entry_max_bars_valid) =
                match timeframe {
                    Timeframe::M1 => (
                        IctSweepBiasMode::OppositeBias,
                        Decimal::new(2, 1),
                        Decimal::new(15, 1),
                        10,
                    ),
                    Timeframe::M5 => (
                        IctSweepBiasMode::SameBias,
                        Decimal::new(1, 1),
                        Decimal::new(8, 1),
                        20,
                    ),
                    Timeframe::M30 => (
                        IctSweepBiasMode::OppositeBias,
                        Decimal::new(1, 1),
                        Decimal::new(1, 0),
                        20,
                    ),
                    _ => (
                        IctSweepBiasMode::OppositeBias,
                        Decimal::new(1, 1),
                        Decimal::new(8, 1),
                        10,
                    ),
                };
            PlanParameters::Ict(IctParameters {
                higher_timeframe,
                fvg_selection: IctFvgSelection::NearestAfterDisplacement,
                sweep_bias_mode,
                atr_period: 14,
                atr_buffer_multiplier,
                minimum_rr,
                bias_max_bars_valid: 20,
                sweep_max_bars_valid: 10,
                displacement_max_bars_valid: 5,
                entry_max_bars_valid,
            })
        }
        PlanKind::Smc => {
            let (htf_mode, minimum_rr, atr_buffer_multiplier) =
                if matches!(timeframe, Timeframe::M5 | Timeframe::M15) {
                    (
                        HtfFilterMode::Strict,
                        Decimal::new(15, 1),
                        Decimal::new(2, 1),
                    )
                } else {
                    (
                        HtfFilterMode::Weighted,
                        Decimal::new(1, 0),
                        Decimal::new(1, 1),
                    )
                };
            PlanParameters::Smc(SmcParameters {
                higher_timeframe,
                htf_mode: if higher_timeframe.is_none() {
                    HtfFilterMode::Off
                } else {
                    htf_mode
                },
                weighted_countertrend_penalty: Decimal::new(2, 1),
                atr_period: 14,
                atr_buffer_multiplier,
                minimum_rr,
                max_bars_valid: 10,
            })
        }
        PlanKind::Rsi => {
            let (oversold_threshold, overbought_threshold, atr_buffer_multiplier, target_rr) =
                match timeframe {
                    Timeframe::M1 => (
                        Decimal::new(35, 0),
                        Decimal::new(65, 0),
                        Decimal::new(2, 1),
                        Decimal::new(15, 1),
                    ),
                    Timeframe::H4 => (
                        Decimal::new(35, 0),
                        Decimal::new(65, 0),
                        Decimal::new(35, 2),
                        Decimal::new(125, 2),
                    ),
                    _ => (
                        Decimal::new(30, 0),
                        Decimal::new(70, 0),
                        Decimal::new(2, 1),
                        Decimal::new(15, 1),
                    ),
                };
            PlanParameters::Rsi(RsiPlanParameters {
                rsi_period: 14,
                oversold_threshold,
                overbought_threshold,
                divergence_required: false,
                minimum_pivot_distance: 2,
                maximum_pivot_distance: 20,
                confirmed_swing_lookback: 50,
                atr_period: 14,
                atr_buffer_multiplier,
                target_rr,
                max_bars_valid: 10,
                reversal_confirmation_bars: 2,
            })
        }
        PlanKind::Quasimodo => {
            let (
                tolerance_atr_multiplier,
                atr_buffer_multiplier,
                max_structure_age_bars,
                max_entry_distance_atr,
            ) = match timeframe {
                Timeframe::M5 => (
                    Decimal::new(2, 1),
                    Decimal::new(2, 1),
                    5,
                    Decimal::new(5, 0),
                ),
                Timeframe::M15 => (
                    Decimal::new(5, 1),
                    Decimal::new(35, 2),
                    10,
                    Decimal::new(3, 0),
                ),
                Timeframe::M30 => (
                    Decimal::new(35, 2),
                    Decimal::new(2, 1),
                    10,
                    Decimal::new(5, 0),
                ),
                _ => (
                    Decimal::new(5, 1),
                    Decimal::new(35, 2),
                    10,
                    Decimal::new(5, 0),
                ),
            };
            PlanParameters::Quasimodo(QuasimodoParameters {
                minimum_zone_ticks: 5,
                tolerance_atr_multiplier,
                atr_period: 14,
                atr_buffer_multiplier,
                minimum_rr: Decimal::new(15, 1),
                max_bars_valid: 10,
                max_structure_age_bars,
                max_entry_distance_atr,
            })
        }
        PlanKind::TrendBreakout => {
            let (
                higher_timeframe,
                minimum_adx,
                initial_stop_atr_multiplier,
                target_rr,
                trailing_activation_r,
            ) = match timeframe {
                Timeframe::M1 => (
                    higher_timeframe,
                    Decimal::new(20, 0),
                    Decimal::new(2, 0),
                    Decimal::new(2, 0),
                    Decimal::new(75, 2),
                ),
                Timeframe::M5 => (
                    higher_timeframe,
                    Decimal::new(20, 0),
                    Decimal::new(15, 1),
                    Decimal::new(15, 1),
                    Decimal::new(75, 2),
                ),
                Timeframe::M15 => (
                    None,
                    Decimal::new(20, 0),
                    Decimal::new(2, 0),
                    Decimal::new(2, 0),
                    Decimal::new(75, 2),
                ),
                Timeframe::H1 => (
                    None,
                    Decimal::new(25, 0),
                    Decimal::new(2, 0),
                    Decimal::new(15, 1),
                    Decimal::new(75, 2),
                ),
                Timeframe::H4 => (
                    None,
                    Decimal::new(20, 0),
                    Decimal::new(15, 1),
                    Decimal::new(2, 0),
                    Decimal::new(75, 2),
                ),
                _ => (
                    higher_timeframe,
                    Decimal::new(25, 0),
                    Decimal::new(2, 0),
                    Decimal::new(2, 0),
                    Decimal::new(1, 0),
                ),
            };
            PlanParameters::TrendBreakout(TrendBreakoutParameters {
                donchian_period: 20,
                ema_period: 50,
                adx_period: 14,
                minimum_adx,
                higher_timeframe,
                atr_period: 14,
                initial_stop_atr_multiplier,
                target_rr,
                trailing_activation_r,
                chandelier_period: 22,
                chandelier_atr_multiplier: Decimal::new(3, 0),
            })
        }
        PlanKind::MeanReversion => {
            let (extreme_zscore, maximum_adx, maximum_bandwidth, minimum_rr, atr_buffer_multiplier) =
                match timeframe {
                    Timeframe::M1 => (
                        Decimal::new(15, 1),
                        Decimal::new(20, 0),
                        Decimal::new(5, 2),
                        Decimal::new(1, 0),
                        Decimal::new(1, 1),
                    ),
                    Timeframe::M5 => (
                        Decimal::new(15, 1),
                        Decimal::new(20, 0),
                        Decimal::new(5, 2),
                        Decimal::new(8, 1),
                        Decimal::new(5, 2),
                    ),
                    Timeframe::M15 | Timeframe::H4 => (
                        Decimal::new(15, 1),
                        Decimal::new(25, 0),
                        Decimal::new(5, 2),
                        Decimal::new(1, 0),
                        Decimal::new(1, 1),
                    ),
                    _ => (
                        Decimal::new(2, 0),
                        Decimal::new(25, 0),
                        Decimal::new(5, 2),
                        Decimal::new(1, 0),
                        Decimal::new(2, 1),
                    ),
                };
            PlanParameters::MeanReversion(MeanReversionParameters {
                bollinger_period: 20,
                bollinger_deviation: Decimal::new(2, 0),
                zscore_period: 20,
                extreme_zscore,
                adx_period: 14,
                maximum_adx,
                maximum_bandwidth,
                atr_period: 14,
                atr_buffer_multiplier,
                minimum_rr,
                max_bars_valid: 10,
            })
        }
        // Pre-registered values, identical on every timeframe (specs of 2026-11-07).
        PlanKind::SessionBreakout => PlanParameters::SessionBreakout(SessionBreakoutParameters {
            range_start_minute_utc: 0,
            range_end_minute_utc: 420,
            decision_end_minute_utc: 720,
            time_exit_minute_utc: 1200,
            atr_period: 14,
            minimum_normalized_width: Decimal::new(5, 1),
            maximum_normalized_width: Decimal::new(2, 0),
            target_rr: Decimal::new(2, 0),
            minimum_entry_rr: Decimal::ONE,
        }),
        PlanKind::VolatilitySqueeze => {
            PlanParameters::VolatilitySqueeze(VolatilitySqueezeParameters {
                bollinger_period: 20,
                bollinger_deviation: Decimal::new(2, 0),
                keltner_atr_multiplier: Decimal::new(15, 1),
                atr_period: 14,
                minimum_squeeze_bars: 6,
                breakout_window_bars: 3,
                breakout_margin_atr: Decimal::new(1, 1),
                stop_buffer_atr: Decimal::new(5, 1),
                target_rr: Decimal::new(2, 0),
                max_bars_to_tp1: 20,
            })
        }
    }
}

pub(super) fn next_higher_timeframe(timeframe: Timeframe) -> Option<Timeframe> {
    match timeframe {
        Timeframe::M1 => Some(Timeframe::M5),
        Timeframe::M5 => Some(Timeframe::M15),
        Timeframe::M15 => Some(Timeframe::M30),
        Timeframe::M30 => Some(Timeframe::H1),
        Timeframe::H1 => Some(Timeframe::H4),
        Timeframe::H4 => Some(Timeframe::D1),
        Timeframe::D1 => None,
    }
}

pub(super) fn runtime_bootstrap_gate() -> ValidationGate {
    ValidationGate {
        chronological_split: true,
        out_of_sample_complete: true,
        sample_size: 1,
        minimum_sample_size: 1,
        unresolved_gap_count: 0,
        metrics: json!({"source": "runtime_bootstrap"}),
        evaluation_start: 0,
        evaluation_end: 1,
    }
}

fn catalog_entry(plan: PlanKind, timeframe: Timeframe) -> RuntimeProfileCatalogEntry {
    RuntimeProfileCatalogEntry {
        plan,
        timeframe,
        enabled: !research_only(plan),
        algorithm_version: runtime_algorithm_version(plan),
        parameter_version: runtime_parameter_version(plan, timeframe),
        parameters: default_parameters(plan, timeframe),
        profit_protection: ProfitProtectionParameters::default(),
        position_management: runtime_position_management(plan, timeframe),
        required_timeframes: if plan == PlanKind::SessionBreakout {
            Vec::new()
        } else {
            next_higher_timeframe(timeframe).into_iter().collect()
        },
        minimum_history_bars: runtime_minimum_history_bars(plan, timeframe),
        warmup_bars: 50,
    }
}

/// Research candidates stay disabled in production until they pass the sealed
/// test window and the vault.
const fn research_only(plan: PlanKind) -> bool {
    matches!(
        plan,
        PlanKind::SessionBreakout | PlanKind::VolatilitySqueeze
    )
}

fn runtime_position_management(
    _plan: PlanKind,
    _target_timeframe: Timeframe,
) -> PositionManagementParameters {
    PositionManagementParameters::default()
}

fn runtime_algorithm_version(plan: PlanKind) -> String {
    match plan {
        PlanKind::Rsi => "RSI-runtime-v4".to_owned(),
        PlanKind::TrendBreakout => "TREND_BREAKOUT-runtime-v4".to_owned(),
        PlanKind::Quasimodo => "QUASIMODO-runtime-v4".to_owned(),
        PlanKind::Ict => "ICT-runtime-v4".to_owned(),
        PlanKind::Smc => "SMC-runtime-v4".to_owned(),
        PlanKind::SessionBreakout => "SESSION_BREAKOUT-runtime-v1".to_owned(),
        PlanKind::VolatilitySqueeze => "VOLATILITY_SQUEEZE-runtime-v1".to_owned(),
        _ => format!("{}-runtime-v3", plan.as_str()),
    }
}

fn runtime_parameter_version(plan: PlanKind, target_timeframe: Timeframe) -> String {
    let version = match plan {
        PlanKind::Rsi
            if matches!(
                target_timeframe,
                Timeframe::M1 | Timeframe::M5 | Timeframe::H4
            ) =>
        {
            "v3"
        }
        PlanKind::Quasimodo
            if matches!(
                target_timeframe,
                Timeframe::M5 | Timeframe::M15 | Timeframe::M30
            ) =>
        {
            "v3"
        }
        PlanKind::Ict if target_timeframe == Timeframe::M5 => "v4",
        PlanKind::Ict if matches!(target_timeframe, Timeframe::M1 | Timeframe::M30) => "v3",
        PlanKind::Rsi | PlanKind::Quasimodo => "v2",
        PlanKind::Smc
            if matches!(
                target_timeframe,
                Timeframe::M5 | Timeframe::M15 | Timeframe::D1
            ) =>
        {
            "v3"
        }
        PlanKind::Smc => "v2",
        PlanKind::Ict => "v2",
        PlanKind::TrendBreakout
            if matches!(
                target_timeframe,
                Timeframe::M1
                    | Timeframe::M5
                    | Timeframe::M15
                    | Timeframe::M30
                    | Timeframe::H1
                    | Timeframe::H4
            ) =>
        {
            "v2"
        }
        PlanKind::MeanReversion if matches!(target_timeframe, Timeframe::M1 | Timeframe::H4) => {
            "v3"
        }
        PlanKind::MeanReversion if matches!(target_timeframe, Timeframe::M5 | Timeframe::M15) => {
            "v2"
        }
        PlanKind::TrendBreakout
        | PlanKind::MeanReversion
        | PlanKind::SessionBreakout
        | PlanKind::VolatilitySqueeze => "v1",
    };
    format!(
        "{}-{}-runtime-{version}-pp1-pc1",
        plan.as_str(),
        target_timeframe.as_str()
    )
}

fn runtime_minimum_history_bars(plan: PlanKind, target_timeframe: Timeframe) -> usize {
    if plan == PlanKind::Ict && matches!(target_timeframe, Timeframe::H4 | Timeframe::D1) {
        150
    } else {
        200
    }
}

fn profile_matches(actual: &MaterializedProfile, expected: &MaterializedProfile) -> bool {
    actual.key == expected.key
        && actual.enabled == expected.enabled
        && actual.versions == expected.versions
        && actual.parameters == expected.parameters
        && actual.profit_protection == expected.profit_protection
        && actual.position_management == expected.position_management
        && actual.required_timeframes == expected.required_timeframes
        && actual.minimum_history_bars == expected.minimum_history_bars
        && actual.warmup_bars == expected.warmup_bars
}
