use analysis_core::{
    ConfirmedSwing, LiquidityPool, LiquiditySide, StructureEvent, StructureEventKind, SwingKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HtfAlignment, HtfContext, PlanFeedback, PlanKind, PlanParameters,
    PlanProfileKey, PrimitiveVersion, QuasimodoParameters, Readiness, SignalDefinition,
    SignalEventKind, TimeframeFacts, TradeState, VersionIdentity,
};
use analysis_plans::{PlanEvaluator, PlanRuntimeState, QuasimodoEvaluator};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn id(kind: FactKind, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M1,
        kind,
        confirmed_at,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    }
}

fn fact<T: Serialize>(id: FactId, value: T) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id,
        value: serde_json::to_value(value).unwrap(),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn swing(kind: SwingKind, ordinal: u32, confirmed_at: i64, price: Decimal) -> ConfirmedSwing {
    let fact_kind = if kind == SwingKind::High {
        FactKind::SwingHigh
    } else {
        FactKind::SwingLow
    };
    let id = id(fact_kind, ordinal, confirmed_at);
    ConfirmedSwing {
        id,
        kind,
        label: None,
        price,
        pivot_open_time: confirmed_at - 30_000,
        confirmed_at,
        source_start_time: confirmed_at - 60_000,
        source_end_time: confirmed_at,
    }
}

fn profile(minimum_rr: Decimal) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Quasimodo,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(Uuid::now_v7(), "qm-v1", "qm-1m-v1").unwrap(),
        parameters: PlanParameters::Quasimodo(QuasimodoParameters {
            minimum_zone_ticks: 5,
            tolerance_atr_multiplier: Decimal::new(1, 1),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr,
            max_bars_valid: 5,
            max_structure_age_bars: 5,
            max_entry_distance_atr: Decimal::new(5, 0),
        }),
        required_timeframes: Vec::new(),
        minimum_history_bars: 100,
        warmup_bars: 20,
    }
}

fn snapshot(direction: Direction) -> CanonicalMarketSnapshot {
    let (shoulder, head, neckline, target_side) = if direction == Direction::Sell {
        (
            swing(SwingKind::High, 1, 60_000, Decimal::new(4_100, 0)),
            swing(SwingKind::High, 2, 120_000, Decimal::new(4_120, 0)),
            swing(SwingKind::Low, 3, 90_000, Decimal::new(4_080, 0)),
            LiquiditySide::SellSide,
        )
    } else {
        (
            swing(SwingKind::Low, 1, 60_000, Decimal::new(4_100, 0)),
            swing(SwingKind::Low, 2, 120_000, Decimal::new(4_080, 0)),
            swing(SwingKind::High, 3, 90_000, Decimal::new(4_120, 0)),
            LiquiditySide::BuySide,
        )
    };
    let break_id = id(FactKind::Structure, 4, 180_000);
    let target_id = id(FactKind::Liquidity, 5, 180_000);
    let target_level = if direction == Direction::Sell {
        Decimal::new(4_050, 0)
    } else {
        Decimal::new(4_150, 0)
    };
    let facts = vec![
        fact(shoulder.id.clone(), shoulder),
        fact(head.id.clone(), head),
        fact(neckline.id.clone(), neckline.clone()),
        fact(
            break_id.clone(),
            StructureEvent {
                id: break_id,
                kind: StructureEventKind::BreakOfStructure,
                direction,
                broken_swing_id: neckline.id,
                confirmation_candle_open_time: 180_000,
                confirmation_close: if direction == Direction::Sell {
                    Decimal::new(4_070, 0)
                } else {
                    Decimal::new(4_130, 0)
                },
            },
        ),
        fact(
            target_id.clone(),
            LiquidityPool {
                id: target_id,
                side: target_side,
                level: target_level,
                lower_bound: target_level - Decimal::ONE,
                upper_bound: target_level + Decimal::ONE,
                source_swing_ids: Vec::new(),
                lifecycle: FactLifecycle::Confirmed,
            },
        ),
        fact(id(FactKind::Atr, 6, 180_000), json!({"value": "10"})),
        fact(
            id(FactKind::MarketClose, 7, 239_999),
            json!({"value": if direction == Direction::Sell { "4070" } else { "4130" }}),
        ),
    ];
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: 239_999,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe: Timeframe::M1,
            evaluated_closed_candle_time: 239_999,
            facts,
        },
        higher_timeframes: Vec::new(),
        primitive_versions: vec![PrimitiveVersion {
            timeframe: Timeframe::M1,
            primitive: "fixture".to_owned(),
            algorithm_version: "fixture-v1".to_owned(),
            parameter_version: "fixture-v1".to_owned(),
        }],
    }
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
fn confirmed_bearish_and_bullish_patterns_emit_limit_zones_after_break() {
    for direction in [Direction::Sell, Direction::Buy] {
        let profile = profile(Decimal::new(15, 1));
        let signal = QuasimodoEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_eq!(signal.direction, direction);
        assert_eq!(
            signal.execution_instruction,
            ExecutionInstruction::LimitZone
        );
        assert_eq!(signal.effective_at, 240_000);
        let zone = signal.entry_zone.unwrap();
        assert_eq!(zone.low, Decimal::new(40_995, 1));
        assert_eq!(zone.high, Decimal::new(41_005, 1));
        assert!(signal.risk_reward >= Decimal::new(15, 1));
    }
}

#[test]
fn signal_captures_farther_swing_or_liquidity_as_immutable_extension_target() {
    let profile = profile(Decimal::new(15, 1));
    let mut input = snapshot(Direction::Buy);
    let far_id = id(FactKind::Liquidity, 80, 180_001);
    input.target.facts.push(fact(
        far_id.clone(),
        LiquidityPool {
            id: far_id,
            side: LiquiditySide::BuySide,
            level: Decimal::new(4_250, 0),
            lower_bound: Decimal::new(4_249, 0),
            upper_bound: Decimal::new(4_251, 0),
            source_swing_ids: Vec::new(),
            lifecycle: FactLifecycle::Confirmed,
        },
    ));

    let signal = QuasimodoEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap()
        .signal
        .unwrap();

    assert!(signal.reference_levels.iter().any(|level| {
        level.kind == "TP2_EXTENSION_TARGET" && level.price == Decimal::new(4_250, 0)
    }));
}

#[test]
fn runner_emits_buffered_move_target_from_immutable_qm_reference() {
    let profile = profile(Decimal::new(15, 1));
    let mut initial = snapshot(Direction::Buy);
    let far_id = id(FactKind::Liquidity, 80, 180_001);
    initial.target.facts.push(fact(
        far_id.clone(),
        LiquidityPool {
            id: far_id,
            side: LiquiditySide::BuySide,
            level: Decimal::new(4_250, 0),
            lower_bound: Decimal::new(4_249, 0),
            upper_bound: Decimal::new(4_251, 0),
            source_swing_ids: Vec::new(),
            lifecycle: FactLifecycle::Confirmed,
        },
    ));
    let created = QuasimodoEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let mut state = created.next_state;
    state.active_signal.as_mut().unwrap().htf_context = Some(HtfContext {
        higher_timeframe: None,
        direction: Direction::None,
        alignment: HtfAlignment::Neutral,
        source: "NEUTRAL".to_owned(),
    });
    let signal = state.active_signal.as_ref().unwrap().clone();
    let primary_target = signal.primary_target;
    let trade_id = Uuid::now_v7();
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 240_000,
            price: Decimal::new(4_100, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 250_000,
            price: Decimal::new(4_122, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let evaluation = QuasimodoEvaluator
        .evaluate(&profile, &initial, &state, &feedback)
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "QM_SHARED_TARGET_EXTENSION");
    assert_eq!(directive.price, Some(Decimal::new(4_249, 0)));
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
fn execution_plan_uses_quasimodo_head_symmetrically() {
    for (direction, thesis) in [
        (Direction::Sell, Decimal::new(4_120, 0)),
        (Direction::Buy, Decimal::new(4_080, 0)),
    ] {
        let profile = profile(Decimal::new(15, 1));
        let signal = QuasimodoEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
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
fn target_must_satisfy_minimum_rr() {
    let profile = profile(Decimal::new(10, 0));
    assert!(
        QuasimodoEvaluator
            .evaluate(
                &profile,
                &snapshot(Direction::Sell),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn stale_structure_break_does_not_emit_signal() {
    let profile = profile(Decimal::new(15, 1));
    let mut stale = snapshot(Direction::Sell);
    stale.evaluated_closed_candle_time = 600_000;

    assert!(
        QuasimodoEvaluator
            .evaluate(&profile, &stale, &PlanRuntimeState::neutral(&profile), &[],)
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn entry_too_far_from_latest_close_does_not_emit_signal() {
    let profile = profile(Decimal::new(15, 1));
    let mut far = snapshot(Direction::Sell);
    far.target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .unwrap()
        .value = json!({"value": "4200"});

    assert!(
        QuasimodoEvaluator
            .evaluate(&profile, &far, &PlanRuntimeState::neutral(&profile), &[],)
            .unwrap()
            .signal
            .is_none()
    );
}

fn active_fixture() -> (
    MaterializedProfile,
    CanonicalMarketSnapshot,
    PlanRuntimeState,
) {
    let profile = profile(Decimal::new(15, 1));
    let snapshot = snapshot(Direction::Sell);
    let evaluation = QuasimodoEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    (profile, snapshot, evaluation.next_state)
}

#[test]
fn head_break_and_expiry_cancel_unentered_pattern() {
    let (profile, mut head_break, state) = active_fixture();
    let break_id = id(FactKind::Structure, 99, 240_000);
    head_break.target.facts.push(fact(
        break_id.clone(),
        StructureEvent {
            id: break_id,
            kind: StructureEventKind::BreakOfStructure,
            direction: Direction::Buy,
            broken_swing_id: id(FactKind::SwingHigh, 98, 230_000),
            confirmation_candle_open_time: 240_000,
            confirmation_close: Decimal::new(4_125, 0),
        },
    ));
    head_break.evaluated_closed_candle_time = 299_999;
    let cancelled = QuasimodoEvaluator
        .evaluate(&profile, &head_break, &state, &[])
        .unwrap();
    assert_eq!(cancelled.directives[0].kind, DirectiveKind::CancelEntry);
    assert_eq!(cancelled.events[0].kind, SignalEventKind::Invalidated);

    let (profile, mut expired, state) = active_fixture();
    expired.evaluated_closed_candle_time =
        state.active_signal.as_ref().unwrap().expires_at.unwrap();
    let expired = QuasimodoEvaluator
        .evaluate(&profile, &expired, &state, &[])
        .unwrap();
    assert_eq!(expired.directives[0].kind, DirectiveKind::ExpireEntry);

    let (profile, mut swept, state) = active_fixture();
    swept
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Liquidity)
        .unwrap()
        .lifecycle = FactLifecycle::Swept;
    let swept = QuasimodoEvaluator
        .evaluate(&profile, &swept, &state, &[])
        .unwrap();
    assert_eq!(swept.directives[0].kind, DirectiveKind::CancelEntry);
}

#[test]
fn opposite_structure_exits_open_trade() {
    let (profile, mut snapshot, state) = active_fixture();
    let thesis = state
        .active_signal
        .as_ref()
        .unwrap()
        .execution_plan
        .as_ref()
        .unwrap()
        .thesis_invalidation;
    snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .unwrap()
        .value = json!({"value": (thesis + Decimal::ONE).to_string()});
    let break_id = id(FactKind::Structure, 99, 240_000);
    snapshot.target.facts.push(fact(
        break_id.clone(),
        StructureEvent {
            id: break_id,
            kind: StructureEventKind::BreakOfStructure,
            direction: Direction::Buy,
            broken_swing_id: id(FactKind::SwingHigh, 98, 230_000),
            confirmation_candle_open_time: 240_000,
            confirmation_close: Decimal::new(4_125, 0),
        },
    ));
    snapshot.evaluated_closed_candle_time = 299_999;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let evaluation = QuasimodoEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &state,
            &[PlanFeedback::ActualEntry {
                signal_id,
                virtual_trade_id: Uuid::now_v7(),
                time: 250_000,
                price: Decimal::new(4_100, 0),
            }],
        )
        .unwrap();
    assert_eq!(evaluation.next_state.trade_state, TradeState::Open);
    assert_eq!(evaluation.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(
        evaluation.directives[0].reason,
        "QM_OPPOSITE_STRUCTURE_BREAK"
    );
}

#[test]
fn runner_health_qm_thesis_close_emits_one_emergency_exit() {
    let (profile, mut invalid, state) = active_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let thesis = signal.execution_plan.as_ref().unwrap().thesis_invalidation;
    let trade_id = Uuid::now_v7();
    invalid
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .unwrap()
        .value = json!({"value": (thesis + Decimal::ONE).to_string()});
    invalid.evaluated_closed_candle_time = 299_999;
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id: trade_id,
            time: 240_000,
            price: Decimal::new(4_100, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id: trade_id,
            time: 260_000,
            price: Decimal::new(4_050, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let managed = QuasimodoEvaluator
        .evaluate(&profile, &invalid, &state, &feedback)
        .unwrap();

    assert_eq!(managed.directives.len(), 1);
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(managed.directives[0].reason, "QM_THESIS_INVALIDATED");
}

#[test]
fn weak_head_shoulder_separation_is_rejected() {
    let profile = profile(Decimal::new(15, 1));
    let mut input = snapshot(Direction::Sell);
    // ATR=10 → need >= 5 separation; force head almost equal to shoulder.
    if let Some(fact) = input
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::SwingHigh && fact.id.ordinal == 2)
    {
        fact.value["price"] = json!("4102");
    }
    let evaluation = QuasimodoEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap();
    assert!(evaluation.signal.is_none());
}

#[test]
fn counter_htf_structure_blocks_new_qm_entry() {
    let profile = profile(Decimal::new(15, 1));
    let mut input = snapshot(Direction::Sell);
    let htf_structure_id = FactId {
        symbol: symbol(),
        timeframe: Timeframe::M5,
        kind: FactKind::Structure,
        confirmed_at: 180_000,
        ordinal: 1,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    };
    let htf_swing_id = FactId {
        symbol: symbol(),
        timeframe: Timeframe::M5,
        kind: FactKind::SwingLow,
        confirmed_at: 120_000,
        ordinal: 1,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    };
    input.higher_timeframes = vec![TimeframeFacts {
        timeframe: Timeframe::M5,
        evaluated_closed_candle_time: 239_999,
        facts: vec![fact(
            htf_structure_id.clone(),
            StructureEvent {
                id: htf_structure_id,
                kind: StructureEventKind::BreakOfStructure,
                direction: Direction::Buy, // counter to sell QM
                broken_swing_id: htf_swing_id,
                confirmation_candle_open_time: 180_000,
                confirmation_close: Decimal::new(4_150, 0),
            },
        )],
    }];
    let evaluation = QuasimodoEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap();
    assert!(evaluation.signal.is_none());
}
