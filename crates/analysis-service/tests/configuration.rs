use analysis_domain::{
    HtfFilterMode, IctParameters, MeanReversionParameters, PlanKind, PlanParameters,
    PlanProfileKey, QuasimodoParameters, ReleaseMode, RsiPlanParameters, SessionBreakoutParameters,
    SmcParameters, TrendBreakoutParameters, VersionIdentity, VolatilitySqueezeParameters,
};
use analysis_service::{
    ConfigurationRelease, MaterializedProfile, ReleaseLifecycle, ValidationGate,
    validate_activation, validate_release,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn next_higher(timeframe: Timeframe) -> Option<Timeframe> {
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

fn parameters(plan: PlanKind, timeframe: Timeframe) -> PlanParameters {
    let higher = next_higher(timeframe);
    match plan {
        PlanKind::Ict => PlanParameters::Ict(IctParameters {
            higher_timeframe: higher,
            fvg_selection: analysis_domain::IctFvgSelection::RelatedDisplacement,
            sweep_bias_mode: analysis_domain::IctSweepBiasMode::SameBias,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            bias_max_bars_valid: 20,
            sweep_max_bars_valid: 10,
            displacement_max_bars_valid: 5,
            entry_max_bars_valid: 10,
        }),
        PlanKind::Smc => PlanParameters::Smc(SmcParameters {
            higher_timeframe: higher,
            htf_mode: if matches!(
                timeframe,
                Timeframe::M1 | Timeframe::M5 | Timeframe::M15 | Timeframe::M30
            ) {
                HtfFilterMode::Strict
            } else {
                HtfFilterMode::Weighted
            },
            weighted_countertrend_penalty: Decimal::new(2, 1),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            max_bars_valid: 10,
        }),
        PlanKind::Rsi => PlanParameters::Rsi(RsiPlanParameters {
            rsi_period: 14,
            oversold_threshold: Decimal::new(30, 0),
            overbought_threshold: Decimal::new(70, 0),
            divergence_required: false,
            minimum_pivot_distance: 2,
            maximum_pivot_distance: 20,
            confirmed_swing_lookback: 50,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            target_rr: Decimal::new(2, 0),
            max_bars_valid: 10,
            reversal_confirmation_bars: 2,
        }),
        PlanKind::Quasimodo => PlanParameters::Quasimodo(QuasimodoParameters {
            minimum_zone_ticks: 5,
            tolerance_atr_multiplier: Decimal::new(2, 1),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            max_bars_valid: 10,
            max_structure_age_bars: 10,
            max_entry_distance_atr: Decimal::new(5, 0),
        }),
        PlanKind::TrendBreakout => PlanParameters::TrendBreakout(TrendBreakoutParameters {
            donchian_period: 20,
            ema_period: 50,
            adx_period: 14,
            minimum_adx: Decimal::new(25, 0),
            higher_timeframe: higher,
            atr_period: 14,
            initial_stop_atr_multiplier: Decimal::new(2, 0),
            target_rr: Decimal::new(2, 0),
            trailing_activation_r: Decimal::new(1, 0),
            chandelier_period: 22,
            chandelier_atr_multiplier: Decimal::new(3, 0),
        }),
        PlanKind::MeanReversion => PlanParameters::MeanReversion(MeanReversionParameters {
            bollinger_period: 20,
            bollinger_deviation: Decimal::new(2, 0),
            zscore_period: 20,
            extreme_zscore: Decimal::new(2, 0),
            adx_period: 14,
            maximum_adx: Decimal::new(25, 0),
            maximum_bandwidth: Decimal::new(5, 2),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(1, 0),
            max_bars_valid: 10,
        }),
        PlanKind::SessionBreakout => PlanParameters::SessionBreakout(SessionBreakoutParameters {
            range_start_minute_utc: 0,
            range_end_minute_utc: 420,
            decision_end_minute_utc: 720,
            time_exit_minute_utc: 1200,
            atr_period: 14,
            minimum_normalized_width: Decimal::new(5, 1),
            maximum_normalized_width: Decimal::TWO,
            target_rr: Decimal::TWO,
            minimum_entry_rr: Decimal::ONE,
        }),
        PlanKind::VolatilitySqueeze => {
            PlanParameters::VolatilitySqueeze(VolatilitySqueezeParameters {
                bollinger_period: 20,
                bollinger_deviation: Decimal::TWO,
                keltner_atr_multiplier: Decimal::new(15, 1),
                atr_period: 14,
                minimum_squeeze_bars: 6,
                breakout_window_bars: 3,
                breakout_margin_atr: Decimal::new(1, 1),
                stop_buffer_atr: Decimal::new(5, 1),
                target_rr: Decimal::TWO,
                max_bars_to_tp1: 20,
            })
        }
    }
}

fn complete_release(mode: ReleaseMode) -> ConfigurationRelease {
    let release_id = Uuid::now_v7();
    let profiles = PlanKind::ALL
        .into_iter()
        .flat_map(|plan| {
            Timeframe::ALL.into_iter().map(move |target_timeframe| {
                let higher = next_higher(target_timeframe);
                MaterializedProfile {
                    key: PlanProfileKey {
                        symbol: symbol(),
                        plan,
                        target_timeframe,
                    },
                    enabled: true,
                    profit_protection: analysis_domain::ProfitProtectionParameters::default(),
                    position_management: analysis_domain::PositionManagementParameters::default(),
                    versions: VersionIdentity::new(
                        release_id,
                        format!("{}-v1", plan.as_str()),
                        format!("{}-{}-v1", plan.as_str(), target_timeframe.as_str()),
                    )
                    .unwrap(),
                    parameters: parameters(plan, target_timeframe),
                    required_timeframes: higher.into_iter().collect(),
                    minimum_history_bars: 200,
                    warmup_bars: 50,
                }
            })
        })
        .collect();
    ConfigurationRelease {
        release_id,
        symbol: symbol(),
        mode,
        lifecycle: ReleaseLifecycle::Draft,
        validation_summary: json!({}),
        profiles,
    }
}

fn passing_gate() -> ValidationGate {
    ValidationGate {
        chronological_split: true,
        out_of_sample_complete: true,
        sample_size: 1_000,
        minimum_sample_size: 100,
        unresolved_gap_count: 0,
        metrics: json!({"profitFactor": "fixture"}),
        evaluation_start: 1,
        evaluation_end: 2,
    }
}

#[test]
fn validates_all_materialized_profiles_and_rejects_duplicates() {
    let mut release = complete_release(ReleaseMode::Research);
    assert_eq!(
        release.profiles.len(),
        PlanKind::ALL.len() * Timeframe::ALL.len()
    );
    assert!(validate_release(&release).is_ok());

    release.profiles[1].key = release.profiles[0].key.clone();
    assert!(validate_release(&release).is_err());
}

#[test]
fn rejects_invalid_period_rr_and_lower_higher_timeframe() {
    let mut release = complete_release(ReleaseMode::Research);
    let PlanParameters::Ict(parameters) = &mut release.profiles[0].parameters else {
        panic!("first profile must be ICT");
    };
    parameters.atr_period = -1;
    assert!(validate_release(&release).is_err());

    let mut release = complete_release(ReleaseMode::Research);
    let profile = release
        .profiles
        .iter_mut()
        .find(|profile| {
            profile.key.plan == PlanKind::TrendBreakout
                && profile.key.target_timeframe == Timeframe::H1
        })
        .unwrap();
    let PlanParameters::TrendBreakout(parameters) = &mut profile.parameters else {
        panic!("profile must be Trend Breakout");
    };
    parameters.target_rr = Decimal::ZERO;
    assert!(validate_release(&release).is_err());

    let mut release = complete_release(ReleaseMode::Research);
    let profile = release
        .profiles
        .iter_mut()
        .find(|profile| {
            profile.key.plan == PlanKind::Ict && profile.key.target_timeframe == Timeframe::H1
        })
        .unwrap();
    profile.required_timeframes = vec![Timeframe::M15];
    assert!(validate_release(&release).is_err());

    let mut release = complete_release(ReleaseMode::Research);
    release.profiles[0].position_management.tp1_fraction = Decimal::ONE;
    assert!(validate_release(&release).is_err());
}

#[test]
fn paper_active_requires_complete_persisted_validation_evidence() {
    let release = complete_release(ReleaseMode::PaperActive);
    let mut gate = passing_gate();
    gate.out_of_sample_complete = false;
    assert!(validate_activation(&release, &gate).is_err());
    assert!(validate_activation(&release, &passing_gate()).is_ok());

    let research = complete_release(ReleaseMode::Research);
    assert!(validate_activation(&research, &ValidationGate::default()).is_ok());
}

#[test]
fn strategy_parameters_have_no_implicit_defaults() {
    let incomplete = json!({"kind": "ICT", "atrPeriod": 14});
    assert!(serde_json::from_value::<PlanParameters>(incomplete).is_err());
}
