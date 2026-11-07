use analysis_core::{
    ConfirmedSwing, Displacement, FairValueGap, FairValueGapDirection, LiquidityPool,
    LiquiditySide, LiquiditySweep, StructureEvent, StructureEventKind, SwingKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, FactId, FactKind,
    FactLifecycle, IctParameters, PlanKind, PlanParameters, PlanProfileKey, PrimitiveVersion,
    Readiness, SignalDefinition, TimeframeFacts, TradeState, VersionIdentity,
};
use analysis_plans::{IctEvaluator, PlanEvaluator, PlanRuntimeState};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn id(kind: FactKind, timeframe: Timeframe, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe,
        kind,
        confirmed_at,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    }
}

fn wrapped<T: Serialize>(
    id: FactId,
    value: T,
    lifecycle: FactLifecycle,
) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id,
        value: serde_json::to_value(value).unwrap(),
        lifecycle,
    }
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
        versions: VersionIdentity::new(release_id, "ict-v1", "ict-1m-v1").unwrap(),
        parameters: PlanParameters::Ict(IctParameters {
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

fn snapshot(stale_fvg: bool, target_level: i64, evaluated: i64) -> CanonicalMarketSnapshot {
    let sweep_id = id(FactKind::LiquiditySweep, Timeframe::M1, 1, 10_000);
    let displacement_id = id(FactKind::Displacement, Timeframe::M1, 2, 20_000);
    let related_displacement = if stale_fvg {
        id(FactKind::Displacement, Timeframe::M1, 99, 5_000)
    } else {
        displacement_id.clone()
    };
    let structure_id = id(FactKind::Structure, Timeframe::M1, 3, 30_000);
    let fvg_id = id(FactKind::FairValueGap, Timeframe::M1, 4, 40_000);
    let swing_id = id(FactKind::SwingLow, Timeframe::M1, 5, 30_000);
    let target_id = id(FactKind::Liquidity, Timeframe::M1, 6, 30_000);
    let atr_id = id(FactKind::Atr, Timeframe::M1, 7, 40_000);
    let target_facts = vec![
        wrapped(
            sweep_id.clone(),
            LiquiditySweep {
                id: sweep_id,
                direction: Direction::Buy,
                source_pool_id: id(FactKind::Liquidity, Timeframe::M1, 0, 5_000),
                source_swing_ids: Vec::new(),
                sweep_candle_open_time: 9_000,
                extreme: Decimal::new(4_050, 0),
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(
            displacement_id.clone(),
            Displacement {
                id: displacement_id.clone(),
                direction: Direction::Buy,
                candle_open_time: 19_000,
                body_atr: Decimal::TWO,
                range_atr: Decimal::new(3, 0),
                low: Decimal::new(4_060, 0),
                high: Decimal::new(4_120, 0),
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(
            structure_id.clone(),
            StructureEvent {
                id: structure_id,
                kind: StructureEventKind::BreakOfStructure,
                direction: Direction::Buy,
                broken_swing_id: swing_id.clone(),
                confirmation_candle_open_time: 29_000,
                confirmation_close: Decimal::new(4_115, 0),
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(
            fvg_id.clone(),
            FairValueGap {
                id: fvg_id,
                direction: FairValueGapDirection::Bullish,
                low: Decimal::new(4_100, 0),
                high: Decimal::new(4_110, 0),
                formation_open_times: [18_000, 19_000, 20_000],
                related_displacement_id: Some(related_displacement),
                lifecycle: FactLifecycle::Confirmed,
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(
            swing_id.clone(),
            ConfirmedSwing {
                id: swing_id,
                kind: SwingKind::Low,
                label: None,
                price: Decimal::new(4_060, 0),
                pivot_open_time: 25_000,
                confirmed_at: 30_000,
                source_start_time: 24_000,
                source_end_time: 30_000,
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(
            target_id.clone(),
            LiquidityPool {
                id: target_id,
                side: LiquiditySide::BuySide,
                level: Decimal::new(target_level, 0),
                lower_bound: Decimal::new(target_level - 1, 0),
                upper_bound: Decimal::new(target_level + 1, 0),
                source_swing_ids: Vec::new(),
                lifecycle: FactLifecycle::Confirmed,
            },
            FactLifecycle::Confirmed,
        ),
        wrapped(atr_id, json!({"value": "10"}), FactLifecycle::Confirmed),
    ];
    let htf_id = id(FactKind::Structure, Timeframe::M5, 0, 35_000);
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: evaluated,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe: Timeframe::M1,
            evaluated_closed_candle_time: evaluated,
            facts: target_facts,
        },
        higher_timeframes: vec![TimeframeFacts {
            timeframe: Timeframe::M5,
            evaluated_closed_candle_time: evaluated,
            facts: vec![wrapped(
                htf_id.clone(),
                StructureEvent {
                    id: htf_id,
                    kind: StructureEventKind::BreakOfStructure,
                    direction: Direction::Buy,
                    broken_swing_id: id(FactKind::SwingHigh, Timeframe::M5, 0, 30_000),
                    confirmation_candle_open_time: 30_000,
                    confirmation_close: Decimal::new(4_100, 0),
                },
                FactLifecycle::Confirmed,
            )],
        }],
        primitive_versions: vec![PrimitiveVersion {
            timeframe: Timeframe::M1,
            primitive: "fixture".to_owned(),
            algorithm_version: "fixture-v1".to_owned(),
            parameter_version: "fixture-v1".to_owned(),
        }],
    }
}

fn sell_snapshot(evaluated: i64) -> CanonicalMarketSnapshot {
    let mut snapshot = snapshot(false, 3_980, evaluated);
    let high_id = id(FactKind::SwingHigh, Timeframe::M1, 5, 30_000);
    for fact in &mut snapshot.target.facts {
        match fact.id.kind {
            FactKind::LiquiditySweep => {
                let mut value: LiquiditySweep = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = Direction::Sell;
                value.extreme = Decimal::new(4_250, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::Displacement => {
                let mut value: Displacement = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = Direction::Sell;
                value.low = Decimal::new(4_080, 0);
                value.high = Decimal::new(4_240, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::Structure => {
                let mut value: StructureEvent = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = Direction::Sell;
                value.broken_swing_id = high_id.clone();
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::FairValueGap => {
                let mut value: FairValueGap = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = FairValueGapDirection::Bearish;
                value.low = Decimal::new(4_140, 0);
                value.high = Decimal::new(4_150, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::SwingLow => {
                fact.id = high_id.clone();
                let mut value: ConfirmedSwing = serde_json::from_value(fact.value.clone()).unwrap();
                value.id = high_id.clone();
                value.kind = SwingKind::High;
                value.price = Decimal::new(4_240, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::Liquidity => {
                let mut value: LiquidityPool = serde_json::from_value(fact.value.clone()).unwrap();
                value.side = LiquiditySide::SellSide;
                fact.value = serde_json::to_value(value).unwrap();
            }
            _ => {}
        }
    }
    let htf = &mut snapshot.higher_timeframes[0].facts[0];
    let mut value: StructureEvent = serde_json::from_value(htf.value.clone()).unwrap();
    value.direction = Direction::Sell;
    htf.value = serde_json::to_value(value).unwrap();
    snapshot
}

fn assert_execution_plan(signal: &SignalDefinition, thesis: Decimal) {
    let plan = signal.execution_plan.as_ref().expect("execution plan");
    assert_eq!(plan.thesis_invalidation, thesis);
    assert_eq!(plan.hard_stop, signal.stop_loss);
    assert_eq!(plan.tp1.trigger_r, Decimal::ONE);
    assert_eq!(plan.tp1.fraction, Decimal::new(5, 1));
    assert!(plan.tp2_zone.low <= signal.primary_target);
    assert!(plan.tp2_zone.high >= signal.primary_target);
    match signal.direction {
        Direction::Buy => {
            assert!(plan.hard_stop < plan.thesis_invalidation);
            assert!(plan.tp2_zone.low < signal.primary_target);
        }
        Direction::Sell => {
            assert!(plan.hard_stop > plan.thesis_invalidation);
            assert!(plan.tp2_zone.high > signal.primary_target);
        }
        Direction::None => unreachable!(),
    }
    assert_eq!(signal.validate_execution_geometry(), Ok(()));
}

#[test]
fn execution_plan_uses_ict_sweep_and_invalidation_extreme_symmetrically() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    for (snapshot, thesis) in [
        (snapshot(false, 4_200, 59_999), Decimal::new(4_050, 0)),
        (sell_snapshot(59_999), Decimal::new(4_250, 0)),
    ] {
        let signal = IctEvaluator
            .evaluate(
                &profile,
                &snapshot,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_execution_plan(&signal, thesis);
    }
}

#[test]
fn related_sequence_emits_limit_zone_signal_with_risk_levels() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    let evaluation = IctEvaluator
        .evaluate(&profile, &snapshot(false, 4_200, 59_999), &state, &[])
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.entry_zone.unwrap().low, Decimal::new(4_100, 0));
    assert_eq!(signal.stop_loss, Decimal::new(4_045, 0));
    assert_eq!(signal.primary_target, Decimal::new(4_200, 0));
    assert_eq!(signal.effective_at, 60_000);
    assert_eq!(signal.release_id, release_id);
}

#[test]
fn real_buy_bos_uses_recent_swing_low_as_invalidation() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    let mut snapshot = snapshot(false, 4_200, 59_999);
    let broken_high_id = id(FactKind::SwingHigh, Timeframe::M1, 88, 28_000);
    snapshot.target.facts.push(wrapped(
        broken_high_id.clone(),
        ConfirmedSwing {
            id: broken_high_id.clone(),
            kind: SwingKind::High,
            label: None,
            price: Decimal::new(4_090, 0),
            pivot_open_time: 24_000,
            confirmed_at: 28_000,
            source_start_time: 23_000,
            source_end_time: 28_000,
        },
        FactLifecycle::Confirmed,
    ));
    let structure = snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Structure)
        .unwrap();
    let mut value: StructureEvent = serde_json::from_value(structure.value.clone()).unwrap();
    value.broken_swing_id = broken_high_id;
    structure.value = serde_json::to_value(value).unwrap();

    let evaluation = IctEvaluator
        .evaluate(&profile, &snapshot, &state, &[])
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.stop_loss, Decimal::new(4_045, 0));
}

#[test]
fn stale_fvg_and_insufficient_rr_do_not_emit() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    assert!(
        IctEvaluator
            .evaluate(&profile, &snapshot(true, 4_200, 59_999), &state, &[])
            .unwrap()
            .signal
            .is_none()
    );
    assert!(
        IctEvaluator
            .evaluate(&profile, &snapshot(false, 4_150, 59_999), &state, &[])
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn nearest_fvg_selection_accepts_unfilled_gap_after_displacement() {
    let release_id = Uuid::now_v7();
    let mut profile = profile(release_id);
    let PlanParameters::Ict(parameters) = &mut profile.parameters else {
        panic!("expected ICT parameters");
    };
    parameters.fvg_selection = analysis_domain::IctFvgSelection::NearestAfterDisplacement;
    parameters.minimum_rr = Decimal::new(8, 1);
    parameters.atr_buffer_multiplier = Decimal::new(1, 1);
    let state = PlanRuntimeState::neutral(&profile);
    let evaluation = IctEvaluator
        .evaluate(&profile, &snapshot(true, 4_200, 59_999), &state, &[])
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.entry_zone.unwrap().low, Decimal::new(4_100, 0));
    assert_eq!(signal.primary_target, Decimal::new(4_200, 0));
}

#[test]
fn top_timeframe_without_htf_uses_target_structure_as_bias() {
    let release_id = Uuid::now_v7();
    let mut profile = profile(release_id);
    let PlanParameters::Ict(parameters) = &mut profile.parameters else {
        panic!("expected ICT parameters");
    };
    parameters.higher_timeframe = None;
    let mut snapshot = snapshot(false, 4_200, 59_999);
    snapshot.higher_timeframes.clear();
    let state = PlanRuntimeState::neutral(&profile);
    let evaluation = IctEvaluator
        .evaluate(&profile, &snapshot, &state, &[])
        .unwrap();
    assert_eq!(evaluation.signal.unwrap().direction, Direction::Buy);
}

#[test]
fn unentered_signal_expires_with_expire_entry_directive() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    let created = IctEvaluator
        .evaluate(&profile, &snapshot(false, 4_200, 59_999), &state, &[])
        .unwrap();
    let expired = IctEvaluator
        .evaluate(
            &profile,
            &snapshot(false, 4_200, 660_000),
            &created.next_state,
            &[],
        )
        .unwrap();
    assert_eq!(expired.directives[0].kind, DirectiveKind::ExpireEntry);
    assert_eq!(
        expired.events[0].kind,
        analysis_domain::SignalEventKind::Expired
    );
}

#[test]
fn bearish_related_sequence_is_symmetric() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let evaluation = IctEvaluator
        .evaluate(
            &profile,
            &sell_snapshot(59_999),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Sell);
    assert_eq!(signal.stop_loss, Decimal::new(4_255, 0));
    assert_eq!(signal.primary_target, Decimal::new(3_980, 0));
}

#[test]
fn no_target_and_pre_entry_invalidations_do_not_leave_a_stale_entry() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    let mut no_target = snapshot(false, 4_200, 59_999);
    no_target
        .target
        .facts
        .retain(|fact| fact.id.kind != FactKind::Liquidity);
    assert!(
        IctEvaluator
            .evaluate(&profile, &no_target, &state, &[])
            .unwrap()
            .signal
            .is_none()
    );

    let created = IctEvaluator
        .evaluate(&profile, &snapshot(false, 4_200, 59_999), &state, &[])
        .unwrap();
    for invalidation in [FactLifecycle::Filled, FactLifecycle::Swept] {
        let mut invalid = snapshot(false, 4_200, 60_999);
        let kind = if invalidation == FactLifecycle::Filled {
            FactKind::FairValueGap
        } else {
            FactKind::Liquidity
        };
        invalid
            .target
            .facts
            .iter_mut()
            .find(|fact| fact.id.kind == kind)
            .unwrap()
            .lifecycle = invalidation;
        assert_eq!(
            IctEvaluator
                .evaluate(&profile, &invalid, &created.next_state, &[])
                .unwrap()
                .directives[0]
                .kind,
            DirectiveKind::CancelEntry
        );
    }
}

#[test]
fn opposing_structure_exits_an_open_trade_at_next_open() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let state = PlanRuntimeState::neutral(&profile);
    let mut created = IctEvaluator
        .evaluate(&profile, &snapshot(false, 4_200, 59_999), &state, &[])
        .unwrap()
        .next_state;
    created.trade_state = TradeState::Open;
    created.active_virtual_trade_id = Some(Uuid::now_v7());
    let thesis = created
        .active_signal
        .as_ref()
        .unwrap()
        .execution_plan
        .as_ref()
        .unwrap()
        .thesis_invalidation;
    let mut reversal = snapshot(false, 4_200, 60_999);
    reversal.target.facts.push(wrapped(
        id(FactKind::MarketClose, Timeframe::M1, 100, 60_600),
        json!({"value": (thesis - Decimal::ONE).to_string()}),
        FactLifecycle::Confirmed,
    ));
    let reversal_id = id(FactKind::Structure, Timeframe::M1, 99, 60_500);
    reversal.target.facts.push(wrapped(
        reversal_id.clone(),
        StructureEvent {
            id: reversal_id,
            kind: StructureEventKind::ChangeOfCharacter,
            direction: Direction::Sell,
            broken_swing_id: id(FactKind::SwingLow, Timeframe::M1, 1, 60_000),
            confirmation_candle_open_time: 60_000,
            confirmation_close: Decimal::new(4_080, 0),
        },
        FactLifecycle::Confirmed,
    ));
    let managed = IctEvaluator
        .evaluate(&profile, &reversal, &created, &[])
        .unwrap();
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(
        managed.directives[0].reason,
        "ICT_POST_ENTRY_STRUCTURE_REVERSAL"
    );
}

#[test]
fn signal_captures_farther_liquidity_as_immutable_extension_target() {
    let profile = profile(Uuid::now_v7());
    let mut input = snapshot(false, 4_200, 59_999);
    let far_id = id(FactKind::Liquidity, Timeframe::M1, 80, 35_000);
    input.target.facts.push(wrapped(
        far_id.clone(),
        LiquidityPool {
            id: far_id,
            side: LiquiditySide::BuySide,
            level: Decimal::new(4_300, 0),
            lower_bound: Decimal::new(4_299, 0),
            upper_bound: Decimal::new(4_301, 0),
            source_swing_ids: Vec::new(),
            lifecycle: FactLifecycle::Confirmed,
        },
        FactLifecycle::Confirmed,
    ));

    let signal = IctEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap()
        .signal
        .unwrap();

    assert!(signal.reference_levels.iter().any(|level| {
        level.kind == "TP2_EXTENSION_TARGET" && level.price == Decimal::new(4_300, 0)
    }));
}

#[test]
fn runner_emits_buffered_move_target_from_immutable_ict_liquidity() {
    let profile = profile(Uuid::now_v7());
    let mut initial = snapshot(false, 4_200, 59_999);
    let far_id = id(FactKind::Liquidity, Timeframe::M1, 80, 35_000);
    initial.target.facts.push(wrapped(
        far_id.clone(),
        LiquidityPool {
            id: far_id,
            side: LiquiditySide::BuySide,
            level: Decimal::new(4_300, 0),
            lower_bound: Decimal::new(4_299, 0),
            upper_bound: Decimal::new(4_301, 0),
            source_swing_ids: Vec::new(),
            lifecycle: FactLifecycle::Confirmed,
        },
        FactLifecycle::Confirmed,
    ));
    let created = IctEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap().clone();
    let primary_target = signal.primary_target;
    let trade_id = Uuid::now_v7();
    let mut progress = initial;
    progress.evaluated_closed_candle_time = 119_999;
    progress.target.evaluated_closed_candle_time = 119_999;
    progress.target.facts.push(wrapped(
        id(FactKind::MarketClose, Timeframe::M1, 81, 60_000),
        json!({"value": "4170"}),
        FactLifecycle::Confirmed,
    ));
    let feedback = [
        analysis_domain::PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 60_000,
            price: Decimal::new(4_105, 0),
        },
        analysis_domain::PlanFeedback::PartialTargetFilled {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 90_000,
            price: Decimal::new(4_162, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let evaluation = IctEvaluator
        .evaluate(&profile, &progress, &created.next_state, &feedback)
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "ICT_SHARED_TARGET_EXTENSION");
    assert_eq!(directive.price, Some(Decimal::new(4_299, 0)));
    assert_eq!(directive.algorithm_version, signal.algorithm_version);
    assert_eq!(directive.parameter_version, signal.parameter_version);
    assert_eq!(
        evaluation
            .next_state
            .active_signal
            .as_ref()
            .unwrap()
            .primary_target,
        primary_target
    );
}

#[test]
fn runner_health_ict_thesis_close_emits_one_emergency_exit() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id);
    let created = IctEvaluator
        .evaluate(
            &profile,
            &snapshot(false, 4_200, 59_999),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let thesis = signal.execution_plan.as_ref().unwrap().thesis_invalidation;
    let trade_id = Uuid::now_v7();
    let mut invalid = snapshot(false, 4_200, 119_999);
    invalid.target.facts.push(wrapped(
        id(FactKind::MarketClose, Timeframe::M1, 100, 60_000),
        json!({"value": (thesis - Decimal::ONE).to_string()}),
        FactLifecycle::Confirmed,
    ));
    let feedback = [
        analysis_domain::PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id: trade_id,
            time: 60_000,
            price: Decimal::new(4_105, 0),
        },
        analysis_domain::PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id: trade_id,
            time: 90_000,
            price: Decimal::new(4_160, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let managed = IctEvaluator
        .evaluate(&profile, &invalid, &created.next_state, &feedback)
        .unwrap();

    assert_eq!(managed.directives.len(), 1);
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(managed.directives[0].reason, "ICT_THESIS_INVALIDATED");
}
