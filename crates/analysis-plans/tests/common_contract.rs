use analysis_domain::MaterializedProfile;
use analysis_domain::{
    Direction, ExecutionInstruction, PlanFeedback, PlanKind, PlanParameters, PlanProfileKey,
    PriceZone, Readiness, ReadinessReason, ReleaseMode, SetupState, SignalDefinition, SignalEvent,
    SignalEventKind, SignalExecutionPlan, TargetLeg, TradeState, VersionIdentity,
};
use analysis_plans::{
    CandidateDecision, IctEvaluator, MeanReversionEvaluator, PlanEvaluator, PlanRuntimeState,
    PositionStage, QuasimodoEvaluator, RejectedCandidate, RsiEvaluator, RunnerHealthAction,
    RunnerHealthInput, SmcEvaluator, TradeHealth, TrendBreakoutEvaluator, append_event,
    apply_feedback, can_reverse, create_signal, evaluate_runner_health, project_state,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn versions(release_id: Uuid) -> VersionIdentity {
    VersionIdentity::new(release_id, "ict-v1", "ict-1m-v1").unwrap()
}

fn profile(release_id: Uuid) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Ict,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: versions(release_id),
        parameters: PlanParameters::Ict(analysis_domain::IctParameters {
            higher_timeframe: Some(Timeframe::M5),
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
        required_timeframes: vec![Timeframe::M5],
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

#[test]
fn execution_plan_changes_all_evaluator_algorithm_versions() {
    assert_eq!(IctEvaluator.algorithm_version(), "ict-v3");
    assert_eq!(SmcEvaluator.algorithm_version(), "smc-v4");
    assert_eq!(QuasimodoEvaluator.algorithm_version(), "qm-v4");
    assert_eq!(MeanReversionEvaluator.algorithm_version(), "mean-v3");
    assert_eq!(TrendBreakoutEvaluator.algorithm_version(), "trend-v4");
    assert_eq!(RsiEvaluator.algorithm_version(), "rsi-v4");
}

#[test]
fn execution_plan_is_required_for_new_algorithm_identity() {
    let release_id = Uuid::now_v7();
    let mut profile = profile(release_id);
    profile.versions = VersionIdentity::new(release_id, "ict-v3", "ict-1m-v3").unwrap();
    let mut state = PlanRuntimeState::neutral(&profile);
    let mut candidate = signal(Uuid::now_v7(), release_id, Direction::Buy);
    candidate.algorithm_version = "ict-v3".to_owned();
    candidate.parameter_version = "ict-1m-v3".to_owned();

    let CandidateDecision::Rejected(rejected) = create_signal(&mut state, candidate).unwrap()
    else {
        panic!("new algorithm identity must reject a missing execution plan");
    };
    assert_eq!(rejected.reason, "MISSING_EXECUTION_PLAN");
}

#[test]
fn execution_plan_is_required_for_partial_close_runtime_identities() {
    for algorithm_version in [
        "ICT-runtime-v4",
        "SMC-runtime-v4",
        "RSI-runtime-v4",
        "QUASIMODO-runtime-v4",
        "MEAN_REVERSION-runtime-v3",
        "TREND_BREAKOUT-runtime-v4",
    ] {
        let release_id = Uuid::now_v7();
        let mut profile = profile(release_id);
        profile.versions =
            VersionIdentity::new(release_id, algorithm_version, "runtime-pp1-pc1").unwrap();
        let mut state = PlanRuntimeState::neutral(&profile);
        let mut candidate = signal(Uuid::now_v7(), release_id, Direction::Buy);
        candidate.algorithm_version = algorithm_version.to_owned();
        candidate.parameter_version = "runtime-pp1-pc1".to_owned();

        let CandidateDecision::Rejected(rejected) = create_signal(&mut state, candidate).unwrap()
        else {
            panic!("{algorithm_version} must reject a missing execution plan");
        };
        assert_eq!(rejected.reason, "MISSING_EXECUTION_PLAN");
    }
}

#[test]
fn execution_plan_is_required_for_rsi_v5_identities() {
    for algorithm_version in ["rsi-v5", "RSI-runtime-v5"] {
        let release_id = Uuid::now_v7();
        let mut profile = profile(release_id);
        profile.key.plan = PlanKind::Rsi;
        profile.versions =
            VersionIdentity::new(release_id, algorithm_version, "rsi-1m-v5").unwrap();
        let mut state = PlanRuntimeState::neutral(&profile);
        let mut candidate = signal(Uuid::now_v7(), release_id, Direction::Buy);
        candidate.plan = PlanKind::Rsi;
        candidate.algorithm_version = algorithm_version.to_owned();
        candidate.parameter_version = "rsi-1m-v5".to_owned();

        let CandidateDecision::Rejected(rejected) = create_signal(&mut state, candidate).unwrap()
        else {
            panic!("{algorithm_version} must reject a missing execution plan");
        };
        assert_eq!(rejected.reason, "MISSING_EXECUTION_PLAN");
    }
}

fn signal(signal_id: Uuid, release_id: Uuid, direction: Direction) -> SignalDefinition {
    SignalDefinition {
        signal_id,
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at: 60_000,
        direction,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(4_100, 0), Decimal::new(4_110, 0)).unwrap()),
        projected_entry: None,
        stop_loss: Decimal::new(4_050, 0),
        primary_target: Decimal::new(4_200, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::new(75, 2),
        evidence: Vec::new(),
        htf_context: None,
        effective_at: 60_000,
        detected_at: 60_001,
        published_at: 60_002,
        release_id,
        algorithm_version: "ict-v1".to_owned(),
        parameter_version: "ict-1m-v1".to_owned(),
        created_at: 60_000,
        expires_at: Some(120_000),
    }
}

#[test]
fn not_ready_is_not_neutral_projection() {
    let state = PlanRuntimeState::not_ready(
        &profile(Uuid::now_v7()),
        ReadinessReason::InsufficientHistory,
    );
    let projection = project_state(&state, 60_000);
    assert_eq!(
        projection.readiness,
        Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory
        }
    );
    assert_eq!(projection.setup_state, SetupState::Neutral);
    assert_ne!(projection.readiness, Readiness::Ready);
}

#[test]
fn signal_creation_rejects_target_stop_on_wrong_side_of_entry() {
    let release_id = Uuid::now_v7();
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    let mut bad_sell = signal(Uuid::now_v7(), release_id, Direction::Sell);
    bad_sell.entry_zone = None;
    bad_sell.projected_entry = Some(Decimal::new(4_087, 0));
    bad_sell.primary_target = Decimal::new(4_071, 0);
    bad_sell.stop_loss = Decimal::new(4_079, 0);

    assert_eq!(
        create_signal(&mut state, bad_sell.clone()).unwrap(),
        CandidateDecision::Rejected(RejectedCandidate {
            candidate_id: bad_sell.signal_id,
            reason: "INVALID_PRICE_GEOMETRY".to_owned(),
            payload: serde_json::to_value(bad_sell).unwrap(),
        })
    );
    assert_eq!(state.active_signal, None);
    assert_eq!(state.setup_state, SetupState::Neutral);
}

#[test]
fn one_active_slot_rejects_candidates_for_audit() {
    let release_id = Uuid::now_v7();
    let first = signal(Uuid::now_v7(), release_id, Direction::Buy);
    let second = signal(Uuid::now_v7(), release_id, Direction::Buy);
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    state.next_event_sequence = 99;
    assert_eq!(
        create_signal(&mut state, first.clone()).unwrap(),
        CandidateDecision::Accepted
    );
    assert_eq!(state.next_event_sequence, 0);
    let decision = create_signal(&mut state, second.clone()).unwrap();
    assert_eq!(
        decision,
        CandidateDecision::Rejected(RejectedCandidate {
            candidate_id: second.signal_id,
            reason: "ACTIVE_SLOT_OCCUPIED".to_owned(),
            payload: serde_json::to_value(second).unwrap(),
        })
    );
    assert_eq!(state.active_signal, Some(first));
}

#[test]
fn signal_creation_is_immutable_and_events_are_append_only() {
    let release_id = Uuid::now_v7();
    let original = signal(Uuid::now_v7(), release_id, Direction::Buy);
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    create_signal(&mut state, original.clone()).unwrap();

    let mut rewritten = original.clone();
    rewritten.primary_target = Decimal::new(9_999, 0);
    assert!(create_signal(&mut state, rewritten).is_err());

    let event = SignalEvent {
        event_id: Uuid::now_v7(),
        signal_id: original.signal_id,
        sequence: 0,
        kind: SignalEventKind::EntryAvailable,
        effective_at: 60_000,
        recorded_at: 60_001,
        virtual_trade_id: None,
        price: None,
        reason: None,
    };
    append_event(&mut state, &event).unwrap();
    assert!(append_event(&mut state, &event).is_err());
    assert_eq!(state.next_event_sequence, 1);
}

#[test]
fn terminal_setup_event_clears_active_signal_slot() {
    let release_id = Uuid::now_v7();
    let active = signal(Uuid::now_v7(), release_id, Direction::Buy);
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    create_signal(&mut state, active.clone()).unwrap();
    let event = SignalEvent {
        event_id: Uuid::now_v7(),
        signal_id: active.signal_id,
        sequence: state.next_event_sequence,
        kind: SignalEventKind::Expired,
        effective_at: 120_001,
        recorded_at: 120_001,
        virtual_trade_id: None,
        price: None,
        reason: Some("entry expired".to_owned()),
    };

    append_event(&mut state, &event).unwrap();

    assert_eq!(state.setup_state, SetupState::Neutral);
    assert_eq!(state.direction, Direction::None);
    assert_eq!(state.trade_state, TradeState::NotEntered);
    assert_eq!(state.active_signal, None);
    assert_eq!(state.active_virtual_trade_id, None);
}

#[test]
fn release_is_pinned_and_actual_entry_feedback_opens_trade() {
    let release_id = Uuid::now_v7();
    let signal_id = Uuid::now_v7();
    let virtual_trade_id = Uuid::now_v7();
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    create_signal(&mut state, signal(signal_id, release_id, Direction::Buy)).unwrap();
    apply_feedback(
        &mut state,
        &PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time: 60_010,
            price: Decimal::new(4_105, 0),
        },
    )
    .unwrap();
    assert_eq!(state.trade_state, TradeState::Open);
    assert_eq!(state.active_virtual_trade_id, Some(virtual_trade_id));
    assert_eq!(state.versions.release_id, release_id);
}

#[test]
fn runner_health_partial_feedback_activates_runner_exactly_once() {
    let release_id = Uuid::now_v7();
    let signal_id = Uuid::now_v7();
    let virtual_trade_id = Uuid::now_v7();
    let mut active = signal(signal_id, release_id, Direction::Buy);
    active.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_080, 0),
        hard_stop: active.stop_loss,
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_198, 0), Decimal::new(4_200, 0)).unwrap(),
    });
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    create_signal(&mut state, active).unwrap();
    apply_feedback(
        &mut state,
        &PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time: 60_010,
            price: Decimal::new(4_105, 0),
        },
    )
    .unwrap();
    let partial = PlanFeedback::PartialTargetFilled {
        signal_id,
        virtual_trade_id,
        time: 60_020,
        price: Decimal::new(4_160, 0),
        realized_fraction: Decimal::new(5, 1),
        remaining_fraction: Decimal::new(5, 1),
    };

    apply_feedback(&mut state, &partial).unwrap();
    let once = state.clone();
    apply_feedback(&mut state, &partial).unwrap();
    apply_feedback(
        &mut state,
        &PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time: 60_010,
            price: Decimal::new(4_105, 0),
        },
    )
    .unwrap();

    assert_eq!(state, once);
    assert_eq!(state.position_stage, PositionStage::Runner);
    assert_eq!(state.trade_health, TradeHealth::Healthy);
    assert_eq!(state.remaining_fraction, Decimal::new(5, 1));
    assert_eq!(state.realized_r, Decimal::new(5, 1));
    assert_eq!(state.thesis_invalidation, Some(Decimal::new(4_080, 0)));
    assert_eq!(state.tp1_filled_at, Some(60_020));
    assert_eq!(
        state.tp2_zone,
        Some(PriceZone::new(Decimal::new(4_198, 0), Decimal::new(4_200, 0)).unwrap())
    );
}

#[test]
fn terminal_trade_feedback_resets_every_runner_lifecycle_field() {
    let release_id = Uuid::now_v7();
    let signal_id = Uuid::now_v7();
    let virtual_trade_id = Uuid::now_v7();
    let mut active = signal(signal_id, release_id, Direction::Buy);
    active.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_080, 0),
        hard_stop: active.stop_loss,
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_198, 0), Decimal::new(4_200, 0)).unwrap(),
    });
    let mut state = PlanRuntimeState::neutral(&profile(release_id));
    create_signal(&mut state, active).unwrap();
    apply_feedback(
        &mut state,
        &PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time: 60_010,
            price: Decimal::new(4_105, 0),
        },
    )
    .unwrap();
    apply_feedback(
        &mut state,
        &PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            time: 60_020,
            price: Decimal::new(4_160, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    )
    .unwrap();

    apply_feedback(
        &mut state,
        &PlanFeedback::TradeClosed {
            signal_id,
            virtual_trade_id,
            time: 60_030,
            reason: "TARGET".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(state.trade_state, TradeState::Closed);
    assert_eq!(state.position_stage, PositionStage::Legacy);
    assert_eq!(state.trade_health, TradeHealth::Healthy);
    assert_eq!(state.remaining_fraction, Decimal::ONE);
    assert_eq!(state.realized_r, Decimal::ZERO);
    assert_eq!(state.thesis_invalidation, None);
    assert_eq!(state.tp1_filled_at, None);
    assert_eq!(state.tp2_zone, None);
}

#[test]
fn legacy_runtime_json_omitting_all_runner_fields_uses_compatible_defaults() {
    let state = PlanRuntimeState::neutral(&profile(Uuid::now_v7()));
    let mut legacy = serde_json::to_value(&state).unwrap();
    let object = legacy.as_object_mut().unwrap();
    for field in [
        "positionStage",
        "tradeHealth",
        "remainingFraction",
        "realizedR",
        "thesisInvalidation",
        "tp1FilledAt",
        "tp2Zone",
    ] {
        object.remove(field);
    }

    let restored: PlanRuntimeState = serde_json::from_value(legacy).unwrap();

    assert_eq!(restored.position_stage, PositionStage::Legacy);
    assert_eq!(restored.trade_health, TradeHealth::Healthy);
    assert_eq!(restored.remaining_fraction, Decimal::ONE);
    assert_eq!(restored.realized_r, Decimal::ZERO);
    assert_eq!(restored.thesis_invalidation, None);
    assert_eq!(restored.tp1_filled_at, None);
    assert_eq!(restored.tp2_zone, None);
}

#[test]
fn runner_health_weakens_recovers_and_suggests_close_only_while_thesis_is_valid() {
    let baseline = RunnerHealthInput {
        thesis_valid: true,
        previous_close: Some(Decimal::new(4_130, 0)),
        latest_close: Decimal::new(4_125, 0),
        previous_mfe_r: Decimal::new(15, 1),
        current_profit_r: Decimal::new(125, 2),
        adverse_close_count: 1,
        htf_alignment: None,
        weakening_giveback_r: Decimal::new(25, 2),
        reversal_giveback_r: Decimal::new(5, 1),
        reversal_adverse_closes: 2,
    };
    let weakening = evaluate_runner_health(baseline);
    assert_eq!(weakening.health, TradeHealth::Weakening);
    assert_eq!(weakening.action, Some(RunnerHealthAction::ProtectProfit));

    let restored = evaluate_runner_health(RunnerHealthInput {
        latest_close: Decimal::new(4_155, 0),
        current_profit_r: Decimal::new(16, 1),
        ..baseline
    });
    assert_eq!(restored.health, TradeHealth::Healthy);
    assert!(restored.new_mfe);
    assert_eq!(restored.action, None);

    let close = evaluate_runner_health(RunnerHealthInput {
        current_profit_r: Decimal::ONE,
        adverse_close_count: 2,
        ..baseline
    });
    assert_eq!(close.health, TradeHealth::Weakening);
    assert_eq!(close.action, Some(RunnerHealthAction::SuggestClose));

    let invalid = evaluate_runner_health(RunnerHealthInput {
        thesis_valid: false,
        current_profit_r: Decimal::ONE,
        adverse_close_count: 2,
        ..baseline
    });
    assert_eq!(invalid.health, TradeHealth::Invalid);
    assert_eq!(invalid.action, None);
}

#[test]
fn only_trend_breakout_can_reverse() {
    for plan in PlanKind::ALL {
        assert_eq!(can_reverse(plan), plan == PlanKind::TrendBreakout);
    }
}

#[test]
fn common_contract_has_no_release_mode_side_effect() {
    assert_eq!(ReleaseMode::Research.as_str(), "RESEARCH");
    assert_eq!(json!({"mode": "RESEARCH"})["mode"], "RESEARCH");
}
