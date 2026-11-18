use analysis_domain::{
    AnalysisWorkItem, CanonicalMarketSnapshot, CurrentPlanProjection, Direction, DirectiveKind,
    EvaluationIdentity, ExecutionInstruction, HtfAlignment, HtfContext, PlanFeedback, PlanKind,
    PlanParameterError, PlanParameters, PositionManagementParameters, PriceZone, Readiness,
    ReadinessReason, ReleaseMode, RsiPlanParameters, SetupState, SignalDefinition, SignalEventKind,
    SignalExecutionPlan, SignalExecutionPlanError, TargetLeg, TrackingHealth, TradeState,
    VersionIdentity, WorkSource, WorkStatus, default_rsi_reversal_confirmation_bars,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

#[test]
fn profit_protection_parameters_default_and_validate() {
    let defaults: analysis_domain::ProfitProtectionParameters =
        serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(defaults.aligned_activation_r, Decimal::ONE);
    assert_eq!(defaults.counter_activation_r, Decimal::new(5, 1));
    assert_eq!(defaults.lock_step_r, Decimal::new(5, 1));
    assert_eq!(defaults.reversal_giveback_r, Decimal::new(5, 1));
    assert_eq!(defaults.adverse_closes, 2);
    assert_eq!(defaults.validate(), Ok(()));

    let invalid = analysis_domain::ProfitProtectionParameters {
        lock_step_r: Decimal::ZERO,
        ..defaults
    };
    assert_eq!(
        invalid.validate(),
        Err(analysis_domain::PlanParameterError::NonPositiveValue)
    );

    let invalid = analysis_domain::ProfitProtectionParameters {
        adverse_closes: 0,
        ..defaults
    };
    assert_eq!(
        invalid.validate(),
        Err(analysis_domain::PlanParameterError::NonPositiveConfirmationCount)
    );
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn version() -> VersionIdentity {
    VersionIdentity::new(Uuid::from_u128(1), "analysis-v1", "xauusdt-ict-1m-v1").unwrap()
}

fn rsi_parameters_with_confirmation_bars(value: i32) -> RsiPlanParameters {
    RsiPlanParameters {
        rsi_period: 14,
        oversold_threshold: Decimal::new(30, 0),
        overbought_threshold: Decimal::new(70, 0),
        divergence_required: false,
        minimum_pivot_distance: 2,
        maximum_pivot_distance: 20,
        confirmed_swing_lookback: 50,
        atr_period: 14,
        atr_buffer_multiplier: Decimal::new(2, 1),
        target_rr: Decimal::TWO,
        max_bars_valid: 10,
        reversal_confirmation_bars: value,
    }
}

#[test]
fn rsi_confirmation_window_default_is_omitted_from_legacy_json() {
    let parameters =
        rsi_parameters_with_confirmation_bars(default_rsi_reversal_confirmation_bars());
    let value = serde_json::to_value(parameters).unwrap();
    assert!(value.get("reversalConfirmationBars").is_none());
}

#[test]
fn rsi_confirmation_window_must_be_positive() {
    let parameters = PlanParameters::Rsi(rsi_parameters_with_confirmation_bars(0));
    assert_eq!(
        parameters.validate(),
        Err(PlanParameterError::NonPositivePeriod)
    );
}

#[test]
fn execution_plan_defaults_and_legacy_signal_deserialization_are_compatible() {
    let defaults = PositionManagementParameters::default();
    assert_eq!(defaults.zone_buffer_multiplier, Decimal::new(5, 1));
    assert_eq!(defaults.minimum_buffer_ticks, 10);
    assert_eq!(defaults.target_buffer_atr_multiplier, Decimal::new(1, 1));
    assert_eq!(defaults.target_buffer_minimum_ticks, 5);
    assert_eq!(defaults.tp1_r, Decimal::ONE);
    assert_eq!(defaults.tp1_fraction, Decimal::new(5, 1));
    assert_eq!(defaults.weakening_giveback_r, Decimal::new(25, 2));
    assert_eq!(defaults.validate(), Ok(()));

    let legacy: SignalDefinition = serde_json::from_value(serde_json::json!({
        "signalId": Uuid::from_u128(10),
        "symbol": "XAUUSDT",
        "plan": "RSI",
        "targetTimeframe": "1m",
        "evaluatedAt": 1_000,
        "direction": "BUY",
        "executionInstruction": "NEXT_OPEN",
        "entryZone": null,
        "projectedEntry": "4100",
        "stopLoss": "4085",
        "primaryTarget": "4130",
        "referenceLevels": [],
        "riskReward": "2",
        "preEntryValidation": null,
        "confidence": "1",
        "evidence": [],
        "effectiveAt": 1_000,
        "detectedAt": 1_000,
        "publishedAt": 1_000,
        "releaseId": Uuid::from_u128(11),
        "algorithmVersion": "rsi-v3",
        "parameterVersion": "rsi-1m-v3",
        "createdAt": 1_000,
        "expiresAt": null
    }))
    .unwrap();

    assert_eq!(legacy.execution_plan, None);
}

#[test]
fn legacy_materialized_profile_defaults_position_management_and_uses_camel_case_wire_name() {
    let profile: analysis_domain::MaterializedProfile = serde_json::from_value(serde_json::json!({
        "key": {
            "symbol": "XAUUSDT",
            "plan": "RSI",
            "targetTimeframe": "1m"
        },
        "enabled": true,
        "versions": {
            "releaseId": Uuid::from_u128(12),
            "algorithmVersion": "rsi-v3",
            "parameterVersion": "rsi-1m-v3"
        },
        "parameters": {
            "kind": "RSI",
            "parameters": {
                "rsiPeriod": 14,
                "oversoldThreshold": "30",
                "overboughtThreshold": "70",
                "divergenceRequired": false,
                "minimumPivotDistance": 2,
                "maximumPivotDistance": 20,
                "confirmedSwingLookback": 50,
                "atrPeriod": 14,
                "atrBufferMultiplier": "0.2",
                "targetRr": "2",
                "maxBarsValid": 10
            }
        },
        "profitProtection": {
            "alignedActivationR": "1",
            "counterActivationR": "0.5",
            "lockStepR": "0.5",
            "reversalGivebackR": "0.5",
            "adverseCloses": 2
        },
        "requiredTimeframes": [],
        "minimumHistoryBars": 200,
        "warmupBars": 50
    }))
    .unwrap();

    assert_eq!(
        profile.position_management,
        PositionManagementParameters {
            zone_buffer_multiplier: Decimal::new(5, 1),
            minimum_buffer_ticks: 10,
            target_buffer_atr_multiplier: Decimal::new(1, 1),
            target_buffer_minimum_ticks: 5,
            tp1_r: Decimal::ONE,
            tp1_fraction: Decimal::new(5, 1),
            weakening_giveback_r: Decimal::new(25, 2),
        }
    );

    let wire = serde_json::to_value(profile).unwrap();
    assert!(wire.get("positionManagement").is_some());
    assert!(wire.get("position_management").is_none());
}

#[test]
fn position_management_rejects_non_positive_decimals_and_tick_counts() {
    let defaults = PositionManagementParameters::default();
    let invalid_decimals = [
        PositionManagementParameters {
            zone_buffer_multiplier: Decimal::ZERO,
            ..defaults.clone()
        },
        PositionManagementParameters {
            target_buffer_atr_multiplier: Decimal::NEGATIVE_ONE,
            ..defaults.clone()
        },
        PositionManagementParameters {
            tp1_r: Decimal::ZERO,
            ..defaults.clone()
        },
        PositionManagementParameters {
            tp1_fraction: Decimal::NEGATIVE_ONE,
            ..defaults.clone()
        },
        PositionManagementParameters {
            weakening_giveback_r: Decimal::ZERO,
            ..defaults.clone()
        },
    ];
    for parameters in invalid_decimals {
        assert_eq!(
            parameters.validate(),
            Err(PlanParameterError::NonPositiveValue)
        );
    }

    for parameters in [
        PositionManagementParameters {
            minimum_buffer_ticks: 0,
            ..defaults.clone()
        },
        PositionManagementParameters {
            target_buffer_minimum_ticks: 0,
            ..defaults.clone()
        },
    ] {
        assert_eq!(
            parameters.validate(),
            Err(PlanParameterError::NonPositiveTickCount)
        );
    }

    let invalid_fraction = PositionManagementParameters {
        tp1_fraction: Decimal::ONE,
        ..defaults
    };
    assert_eq!(
        invalid_fraction.validate(),
        Err(PlanParameterError::InvalidFraction)
    );
}

#[test]
fn execution_plan_validates_fraction_and_directional_geometry() {
    let plan = SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_090, 0),
        hard_stop: Decimal::new(4_085, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    };
    assert_eq!(
        plan.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Ok(())
    );

    let invalid_fraction = SignalExecutionPlan {
        tp1: TargetLeg {
            fraction: Decimal::ONE,
            ..plan.tp1.clone()
        },
        ..plan.clone()
    };
    assert_eq!(
        invalid_fraction.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::InvalidFraction)
    );

    let non_half_fraction = SignalExecutionPlan {
        tp1: TargetLeg {
            fraction: Decimal::new(4, 1),
            ..plan.tp1.clone()
        },
        ..plan.clone()
    };
    assert_eq!(
        non_half_fraction.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::InvalidFraction)
    );

    let invalid_geometry = SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_080, 0),
        ..plan
    };
    assert_eq!(
        invalid_geometry.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::InvalidGeometry)
    );

    let target_without_execution_buffer = SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_090, 0),
        hard_stop: Decimal::new(4_085, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_130, 0), Decimal::new(4_130, 0)).unwrap(),
    };
    assert_eq!(
        target_without_execution_buffer.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::InvalidGeometry)
    );

    let tp1_beyond_target = SignalExecutionPlan {
        tp1: TargetLeg {
            trigger_r: Decimal::new(3, 0),
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
        ..plan
    };
    assert_eq!(
        tp1_beyond_target.validate(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::Tp1BeyondTarget)
    );

    assert_eq!(
        tp1_beyond_target.validate(
            Direction::None,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
        ),
        Err(SignalExecutionPlanError::MissingDirection)
    );
}

fn sell_execution_plan() -> SignalExecutionPlan {
    SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_105, 0),
        hard_stop: Decimal::new(4_110, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_070, 0), Decimal::new(4_072, 0)).unwrap(),
    }
}

#[test]
fn sell_execution_plan_covers_directional_bounds_and_tp1_edge() {
    let plan = sell_execution_plan();
    let entry = Decimal::new(4_100, 0);
    let target = Decimal::new(4_071, 0);
    assert_eq!(plan.validate(Direction::Sell, entry, target), Ok(()));

    for (invalid, invalid_entry, invalid_target) in [
        (plan.clone(), Decimal::new(4_072, 0), target),
        (plan.clone(), Decimal::new(4_106, 0), target),
        (
            SignalExecutionPlan {
                thesis_invalidation: Decimal::new(4_110, 0),
                ..plan.clone()
            },
            entry,
            target,
        ),
        (plan.clone(), entry, Decimal::new(4_069, 0)),
        (plan.clone(), entry, Decimal::new(4_072, 0)),
    ] {
        assert_eq!(
            invalid.validate(Direction::Sell, invalid_entry, invalid_target),
            Err(SignalExecutionPlanError::InvalidGeometry)
        );
    }

    let tp1_at_target_edge = SignalExecutionPlan {
        tp1: TargetLeg {
            trigger_r: Decimal::new(28, 1),
            ..plan.tp1.clone()
        },
        ..plan.clone()
    };
    assert_eq!(
        tp1_at_target_edge.validate(Direction::Sell, entry, target),
        Ok(())
    );

    let tp1_beyond_target = SignalExecutionPlan {
        tp1: TargetLeg {
            trigger_r: Decimal::new(29, 1),
            ..plan.tp1
        },
        ..plan
    };
    assert_eq!(
        tp1_beyond_target.validate(Direction::Sell, entry, target),
        Err(SignalExecutionPlanError::Tp1BeyondTarget)
    );
}

fn signal_with_execution_plan() -> SignalDefinition {
    serde_json::from_value(serde_json::json!({
        "signalId": Uuid::from_u128(20),
        "symbol": "XAUUSDT",
        "plan": "RSI",
        "targetTimeframe": "1m",
        "evaluatedAt": 1_000,
        "direction": "BUY",
        "executionInstruction": "NEXT_OPEN",
        "entryZone": { "low": "4190", "high": "4210" },
        "projectedEntry": "4100",
        "stopLoss": "4085",
        "primaryTarget": "4130",
        "executionPlan": {
            "thesisInvalidation": "4090",
            "hardStop": "4085",
            "tp1": { "triggerR": "1", "fraction": "0.5" },
            "tp2Zone": { "low": "4128", "high": "4130" }
        },
        "referenceLevels": [],
        "riskReward": "2",
        "preEntryValidation": null,
        "confidence": "1",
        "evidence": [],
        "effectiveAt": 1_000,
        "detectedAt": 1_000,
        "publishedAt": 1_000,
        "releaseId": Uuid::from_u128(21),
        "algorithmVersion": "rsi-v3",
        "parameterVersion": "rsi-1m-v3",
        "createdAt": 1_000,
        "expiresAt": null
    }))
    .unwrap()
}

#[test]
fn signal_execution_geometry_covers_entry_selection_and_compatibility_paths() {
    let signal = signal_with_execution_plan();
    assert_eq!(signal.validate_execution_geometry(), Ok(()));

    let mut midpoint_fallback = signal.clone();
    midpoint_fallback.projected_entry = None;
    midpoint_fallback.entry_zone =
        Some(PriceZone::new(Decimal::new(4_098, 0), Decimal::new(4_102, 0)).unwrap());
    assert_eq!(midpoint_fallback.validate_execution_geometry(), Ok(()));

    let mut hard_stop_mismatch = signal.clone();
    hard_stop_mismatch.stop_loss = Decimal::new(4_084, 0);
    assert_eq!(
        hard_stop_mismatch.validate_execution_geometry(),
        Err(SignalExecutionPlanError::HardStopMismatch)
    );

    let mut missing_entry_reference = signal.clone();
    missing_entry_reference.projected_entry = None;
    missing_entry_reference.entry_zone = None;
    assert_eq!(
        missing_entry_reference.validate_execution_geometry(),
        Err(SignalExecutionPlanError::MissingEntryReference)
    );

    let mut legacy_signal = missing_entry_reference;
    legacy_signal.execution_plan = None;
    assert_eq!(legacy_signal.validate_execution_geometry(), Ok(()));
}

#[test]
fn all_plans_have_stable_order() {
    assert_eq!(
        PlanKind::ALL,
        [
            PlanKind::Ict,
            PlanKind::Smc,
            PlanKind::Rsi,
            PlanKind::Quasimodo,
            PlanKind::TrendBreakout,
            PlanKind::MeanReversion,
            PlanKind::SessionBreakout,
            PlanKind::VolatilitySqueeze,
        ]
    );
    assert_eq!(
        PlanKind::ALL.map(PlanKind::as_str),
        [
            "ICT",
            "SMC",
            "RSI",
            "QUASIMODO",
            "TREND_BREAKOUT",
            "MEAN_REVERSION",
            "SESSION_BREAKOUT",
            "VOLATILITY_SQUEEZE",
        ]
    );
}

#[test]
fn buy_and_sell_are_direction_not_setup_state() {
    assert_eq!(SetupState::EntryAvailable.as_str(), "ENTRY_AVAILABLE");
    assert_eq!(Direction::Buy.as_str(), "BUY");
    assert_eq!(Direction::Sell.as_str(), "SELL");
    assert_eq!(Direction::None.as_str(), "NONE");
}

#[test]
fn htf_context_has_stable_wire_values() {
    let context = HtfContext {
        higher_timeframe: Some(Timeframe::M5),
        direction: Direction::Buy,
        alignment: HtfAlignment::Aligned,
        source: "STRUCTURE".to_owned(),
    };
    let json = serde_json::to_value(context).unwrap();

    assert_eq!(json["higherTimeframe"], "5m");
    assert_eq!(json["direction"], "BUY");
    assert_eq!(json["alignment"], "ALIGNED");
    assert_eq!(json["source"], "STRUCTURE");
}

#[test]
fn price_zone_orders_its_bounds_and_requires_positive_prices() {
    assert!(PriceZone::new(Decimal::new(4101, 0), Decimal::new(4100, 0)).is_err());
    assert!(PriceZone::new(Decimal::ZERO, Decimal::ONE).is_err());
    assert_eq!(
        PriceZone::new(Decimal::new(4100, 0), Decimal::new(4101, 0)).unwrap(),
        PriceZone {
            low: Decimal::new(4100, 0),
            high: Decimal::new(4101, 0),
        }
    );
}

#[test]
fn version_identity_rejects_blank_versions() {
    assert!(VersionIdentity::new(Uuid::from_u128(1), "", "parameter-v1").is_err());
    assert!(VersionIdentity::new(Uuid::from_u128(1), "algorithm-v1", "  ").is_err());
}

#[test]
fn projection_separates_readiness_setup_trade_and_health() {
    let projection = CurrentPlanProjection {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        readiness: Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory,
        },
        setup_state: SetupState::Neutral,
        direction: Direction::None,
        trade_state: TradeState::NotEntered,
        tracking_health: TrackingHealth::Live,
        active_signal_id: None,
        active_virtual_trade_id: None,
        version: version(),
        evaluated_closed_candle_time: 1_782_195_300_000,
    };

    assert_ne!(projection.readiness, Readiness::Ready);
    assert_eq!(projection.setup_state, SetupState::Neutral);
    assert_eq!(projection.trade_state, TradeState::NotEntered);
}

#[test]
fn evaluation_identity_covers_all_deduplication_dimensions() {
    let identity = EvaluationIdentity {
        symbol: symbol(),
        plan: PlanKind::MeanReversion,
        target_timeframe: Timeframe::H1,
        release_id: Uuid::from_u128(5),
        algorithm_version: "mean-reversion-v1".to_owned(),
        parameter_version: "xauusdt-mean-reversion-1h-v2".to_owned(),
        evaluated_closed_candle_time: 1_782_190_800_000,
    };
    assert!(identity.validate().is_ok());

    let mut invalid = identity;
    invalid.evaluated_closed_candle_time = -1;
    assert!(invalid.validate().is_err());
}

#[test]
fn work_item_keeps_source_status_and_lease_state_separate() {
    let item = AnalysisWorkItem {
        work_id: Uuid::from_u128(7),
        symbol: symbol(),
        timeframe: Timeframe::M5,
        canonical_close_time: 1_782_194_999_999,
        release_id: Uuid::from_u128(6),
        source: WorkSource::CatchUp,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: 1_782_195_000_000,
        completed_at: None,
    };
    assert!(item.validate().is_ok());
    assert_eq!(item.source.as_str(), "CATCH_UP");
    assert_eq!(item.status.as_str(), "PENDING");
}

#[test]
fn snapshot_contains_closed_candle_boundary_and_no_signal_fields() {
    let snapshot = CanonicalMarketSnapshot::empty(
        symbol(),
        Timeframe::M15,
        Decimal::new(1, 1),
        1_782_193_500_000,
        Readiness::Ready,
    )
    .unwrap();
    let json = serde_json::to_value(snapshot).unwrap();

    assert_eq!(json["targetTimeframe"], "15m");
    assert!(json.get("signal").is_none());
    assert!(json.get("direction").is_none());
    assert_eq!(json["priceTick"], "0.1");
    assert!(
        CanonicalMarketSnapshot::empty(
            symbol(),
            Timeframe::M15,
            Decimal::ZERO,
            1_782_193_500_000,
            Readiness::Ready,
        )
        .is_err()
    );
}

#[test]
fn execution_and_directive_kinds_have_stable_wire_values() {
    assert_eq!(ExecutionInstruction::LimitZone.as_str(), "LIMIT_ZONE");
    assert_eq!(ExecutionInstruction::NextOpen.as_str(), "NEXT_OPEN");
    assert_eq!(DirectiveKind::MoveStop.as_str(), "MOVE_STOP");
    assert_eq!(DirectiveKind::MoveTarget.as_str(), "MOVE_TARGET");
    assert_eq!(DirectiveKind::SuggestClose.as_str(), "SUGGEST_CLOSE");
    assert_eq!(DirectiveKind::EmergencyExit.as_str(), "EMERGENCY_EXIT");
    assert_eq!(SignalEventKind::EntryAvailable.as_str(), "ENTRY_AVAILABLE");
    assert_eq!(
        DirectiveKind::ReverseAtNextOpen.as_str(),
        "REVERSE_AT_NEXT_OPEN"
    );
    assert_eq!(ReleaseMode::PaperActive.as_str(), "PAPER_ACTIVE");
}

#[test]
fn partial_target_feedback_round_trips_with_stable_identity_and_fractions() {
    let feedback = PlanFeedback::PartialTargetFilled {
        signal_id: Uuid::from_u128(10),
        virtual_trade_id: Uuid::from_u128(11),
        time: 1_782_195_600_000,
        price: Decimal::new(4_110, 0),
        realized_fraction: Decimal::new(5, 1),
        remaining_fraction: Decimal::new(5, 1),
    };

    let json = serde_json::to_value(&feedback).unwrap();
    assert_eq!(json["kind"], "PARTIAL_TARGET_FILLED");
    assert_eq!(
        serde_json::from_value::<PlanFeedback>(json).unwrap(),
        feedback
    );
}
