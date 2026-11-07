use analysis_core::{
    ConfirmedSwing, Displacement, FairValueGap, FairValueGapDirection, LiquidityPool,
    LiquiditySide, StructureEvent, StructureEventKind, SwingKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, FactId, FactKind,
    FactLifecycle, HtfFilterMode, PlanFeedback, PlanKind, PlanParameters, PlanProfileKey,
    PrimitiveVersion, Readiness, SignalDefinition, SignalEventKind, SmcParameters, TimeframeFacts,
    TradeState, VersionIdentity,
};
use analysis_plans::{PlanEvaluator, PlanRuntimeState, SmcEvaluator};
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

fn wrapped<T: Serialize>(id: FactId, value: T) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id,
        value: serde_json::to_value(value).unwrap(),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn profile(release_id: Uuid, mode: HtfFilterMode) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Smc,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(release_id, "smc-v1", "smc-1m-v1").unwrap(),
        parameters: PlanParameters::Smc(SmcParameters {
            higher_timeframe: Some(Timeframe::M5),
            htf_mode: mode,
            weighted_countertrend_penalty: Decimal::new(2, 1),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::new(15, 1),
            max_bars_valid: 10,
        }),
        required_timeframes: vec![Timeframe::M5],
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

fn snapshot(reversed_order: bool, htf_direction: Direction) -> CanonicalMarketSnapshot {
    let swing_id = id(FactKind::SwingLow, Timeframe::M1, 1, 10_000);
    let choch_id = id(
        FactKind::Structure,
        Timeframe::M1,
        2,
        if reversed_order { 30_000 } else { 20_000 },
    );
    let displacement_id = id(FactKind::Displacement, Timeframe::M1, 3, 25_000);
    let bos_id = id(
        FactKind::Structure,
        Timeframe::M1,
        4,
        if reversed_order { 20_000 } else { 30_000 },
    );
    let fvg_id = id(FactKind::FairValueGap, Timeframe::M1, 5, 35_000);
    let target_id = id(FactKind::Liquidity, Timeframe::M1, 6, 35_000);
    let target = TimeframeFacts {
        timeframe: Timeframe::M1,
        evaluated_closed_candle_time: 59_999,
        facts: vec![
            wrapped(
                choch_id.clone(),
                StructureEvent {
                    id: choch_id,
                    kind: StructureEventKind::ChangeOfCharacter,
                    direction: Direction::Buy,
                    broken_swing_id: swing_id.clone(),
                    confirmation_candle_open_time: 20_000,
                    confirmation_close: Decimal::new(4_080, 0),
                },
            ),
            wrapped(
                displacement_id.clone(),
                Displacement {
                    id: displacement_id.clone(),
                    direction: Direction::Buy,
                    candle_open_time: 25_000,
                    body_atr: Decimal::TWO,
                    range_atr: Decimal::new(3, 0),
                    low: Decimal::new(4_060, 0),
                    high: Decimal::new(4_120, 0),
                },
            ),
            wrapped(
                bos_id.clone(),
                StructureEvent {
                    id: bos_id,
                    kind: StructureEventKind::BreakOfStructure,
                    direction: Direction::Buy,
                    broken_swing_id: swing_id.clone(),
                    confirmation_candle_open_time: 30_000,
                    confirmation_close: Decimal::new(4_115, 0),
                },
            ),
            wrapped(
                fvg_id.clone(),
                FairValueGap {
                    id: fvg_id,
                    direction: FairValueGapDirection::Bullish,
                    low: Decimal::new(4_100, 0),
                    high: Decimal::new(4_110, 0),
                    formation_open_times: [24_000, 25_000, 26_000],
                    related_displacement_id: Some(displacement_id),
                    lifecycle: FactLifecycle::Confirmed,
                },
            ),
            wrapped(
                swing_id.clone(),
                ConfirmedSwing {
                    id: swing_id,
                    kind: SwingKind::Low,
                    label: None,
                    price: Decimal::new(4_060, 0),
                    pivot_open_time: 10_000,
                    confirmed_at: 15_000,
                    source_start_time: 9_000,
                    source_end_time: 15_000,
                },
            ),
            wrapped(
                target_id.clone(),
                LiquidityPool {
                    id: target_id,
                    side: LiquiditySide::BuySide,
                    level: Decimal::new(4_200, 0),
                    lower_bound: Decimal::new(4_199, 0),
                    upper_bound: Decimal::new(4_201, 0),
                    source_swing_ids: Vec::new(),
                    lifecycle: FactLifecycle::Confirmed,
                },
            ),
            wrapped(
                id(FactKind::Atr, Timeframe::M1, 7, 35_000),
                json!({"value": "10"}),
            ),
            wrapped(
                id(FactKind::MarketClose, Timeframe::M1, 8, 59_999),
                json!({"value": "4115"}),
            ),
        ],
    };
    let htf_id = id(FactKind::Structure, Timeframe::M5, 1, 35_000);
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: 59_999,
        readiness: Readiness::Ready,
        target,
        higher_timeframes: vec![TimeframeFacts {
            timeframe: Timeframe::M5,
            evaluated_closed_candle_time: 59_999,
            facts: vec![wrapped(
                htf_id.clone(),
                StructureEvent {
                    id: htf_id,
                    kind: StructureEventKind::BreakOfStructure,
                    direction: htf_direction,
                    broken_swing_id: id(FactKind::SwingHigh, Timeframe::M5, 0, 30_000),
                    confirmation_candle_open_time: 30_000,
                    confirmation_close: Decimal::new(4_100, 0),
                },
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

fn sell_snapshot() -> CanonicalMarketSnapshot {
    let mut snapshot = snapshot(false, Direction::Sell);
    let swing_id = id(FactKind::SwingHigh, Timeframe::M1, 1, 10_000);
    for fact in &mut snapshot.target.facts {
        match fact.id.kind {
            FactKind::Structure => {
                let mut value: StructureEvent = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = Direction::Sell;
                value.broken_swing_id = swing_id.clone();
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::Displacement => {
                let mut value: Displacement = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = Direction::Sell;
                value.low = Decimal::new(4_080, 0);
                value.high = Decimal::new(4_140, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::FairValueGap => {
                let mut value: FairValueGap = serde_json::from_value(fact.value.clone()).unwrap();
                value.direction = FairValueGapDirection::Bearish;
                value.low = Decimal::new(4_090, 0);
                value.high = Decimal::new(4_100, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::SwingLow => {
                fact.id = swing_id.clone();
                let mut value: ConfirmedSwing = serde_json::from_value(fact.value.clone()).unwrap();
                value.id = swing_id.clone();
                value.kind = SwingKind::High;
                value.price = Decimal::new(4_140, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::Liquidity => {
                let mut value: LiquidityPool = serde_json::from_value(fact.value.clone()).unwrap();
                value.side = LiquiditySide::SellSide;
                value.level = Decimal::new(4_000, 0);
                value.lower_bound = Decimal::new(3_999, 0);
                value.upper_bound = Decimal::new(4_001, 0);
                fact.value = serde_json::to_value(value).unwrap();
            }
            FactKind::MarketClose => fact.value = json!({"value": "4085"}),
            _ => {}
        }
    }
    snapshot
}

fn with_market_close(mut snapshot: CanonicalMarketSnapshot, close: i64) -> CanonicalMarketSnapshot {
    let fact = snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .expect("market close fact");
    fact.value = json!({"value": close.to_string()});
    snapshot
}

fn signal_for(snapshot: &CanonicalMarketSnapshot) -> Option<SignalDefinition> {
    let profile = profile(Uuid::now_v7(), HtfFilterMode::Off);
    SmcEvaluator
        .evaluate(
            &profile,
            snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
}

/// A complete older buy setup, in the oldest-first order the runtime builds facts.
fn older_buy_setup_facts() -> Vec<ConfirmedFact<serde_json::Value>> {
    let swing_id = id(FactKind::SwingLow, Timeframe::M1, 20, 500);
    let choch_id = id(FactKind::Structure, Timeframe::M1, 21, 1_000);
    let displacement_id = id(FactKind::Displacement, Timeframe::M1, 22, 2_000);
    let bos_id = id(FactKind::Structure, Timeframe::M1, 23, 3_000);
    let fvg_id = id(FactKind::FairValueGap, Timeframe::M1, 24, 2_000);
    vec![
        wrapped(
            swing_id.clone(),
            ConfirmedSwing {
                id: swing_id.clone(),
                kind: SwingKind::Low,
                label: None,
                price: Decimal::new(4_050, 0),
                pivot_open_time: 300,
                confirmed_at: 500,
                source_start_time: 100,
                source_end_time: 500,
            },
        ),
        wrapped(
            choch_id.clone(),
            StructureEvent {
                id: choch_id,
                kind: StructureEventKind::ChangeOfCharacter,
                direction: Direction::Buy,
                broken_swing_id: swing_id.clone(),
                confirmation_candle_open_time: 1_000,
                confirmation_close: Decimal::new(4_060, 0),
            },
        ),
        wrapped(
            displacement_id.clone(),
            Displacement {
                id: displacement_id.clone(),
                direction: Direction::Buy,
                candle_open_time: 2_000,
                body_atr: Decimal::TWO,
                range_atr: Decimal::new(3, 0),
                low: Decimal::new(4_050, 0),
                high: Decimal::new(4_090, 0),
            },
        ),
        wrapped(
            bos_id.clone(),
            StructureEvent {
                id: bos_id,
                kind: StructureEventKind::BreakOfStructure,
                direction: Direction::Buy,
                broken_swing_id: swing_id,
                confirmation_candle_open_time: 3_000,
                confirmation_close: Decimal::new(4_085, 0),
            },
        ),
        wrapped(
            fvg_id.clone(),
            FairValueGap {
                id: fvg_id,
                direction: FairValueGapDirection::Bullish,
                low: Decimal::new(4_070, 0),
                high: Decimal::new(4_080, 0),
                formation_open_times: [1_000, 2_000, 3_000],
                related_displacement_id: Some(displacement_id),
                lifecycle: FactLifecycle::Confirmed,
            },
        ),
    ]
}

#[test]
fn newest_setup_wins_over_an_older_one_still_in_the_window() {
    let mut snapshot = snapshot(false, Direction::Buy);
    snapshot.target.facts.splice(0..0, older_buy_setup_facts());

    let signal = signal_for(&snapshot).expect("signal");

    let zone = signal.entry_zone.expect("entry zone");
    assert_eq!(zone.low, Decimal::new(4_100, 0));
    assert_eq!(zone.high, Decimal::new(4_110, 0));
}

#[test]
fn setup_expires_max_bars_valid_after_its_break_of_structure() {
    // BOS confirmed at 30_000 on M1 with max_bars_valid = 10.
    let last_valid_evaluation = 30_000 + 10 * 60_000;
    for (evaluated, expected) in [
        (last_valid_evaluation, true),
        (last_valid_evaluation + 1, false),
    ] {
        let mut snapshot = snapshot(false, Direction::Buy);
        snapshot.evaluated_closed_candle_time = evaluated;

        assert_eq!(
            signal_for(&snapshot).is_some(),
            expected,
            "evaluated at {evaluated}"
        );
    }
}

#[test]
fn signal_requires_price_between_the_gap_and_the_target() {
    // Buy gap 4100-4110 with target 4200; sell gap 4090-4100 with target 4000.
    for (close, expected) in [
        (4_105, true),
        (4_150, true),
        (4_099, false),
        (4_200, false),
        (4_250, false),
    ] {
        let snapshot = with_market_close(snapshot(false, Direction::Buy), close);
        assert_eq!(
            signal_for(&snapshot).is_some(),
            expected,
            "buy with close {close}"
        );
    }
    for (close, expected) in [
        (4_095, true),
        (4_050, true),
        (4_101, false),
        (4_000, false),
        (3_950, false),
    ] {
        let snapshot = with_market_close(sell_snapshot(), close);
        assert_eq!(
            signal_for(&snapshot).is_some(),
            expected,
            "sell with close {close}"
        );
    }
}

#[test]
fn filled_related_gap_is_not_signalled() {
    let mut snapshot = snapshot(false, Direction::Buy);
    let fact = snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::FairValueGap)
        .expect("gap fact");
    let mut gap: FairValueGap = serde_json::from_value(fact.value.clone()).unwrap();
    gap.lifecycle = FactLifecycle::Filled;
    fact.value = serde_json::to_value(gap).unwrap();

    assert!(signal_for(&snapshot).is_none());
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
fn execution_plan_uses_smc_invalidation_swing_symmetrically() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, HtfFilterMode::Strict);
    for (snapshot, thesis) in [
        (snapshot(false, Direction::Buy), Decimal::new(4_060, 0)),
        (sell_snapshot(), Decimal::new(4_140, 0)),
    ] {
        let signal = SmcEvaluator
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
fn ordered_choch_then_bos_emits_related_fvg_signal() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, HtfFilterMode::Strict);
    let evaluation = SmcEvaluator
        .evaluate(
            &profile,
            &snapshot(false, Direction::Buy),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.stop_loss, Decimal::new(4_055, 0));
    assert_eq!(signal.primary_target, Decimal::new(4_200, 0));
    assert!(signal.evidence.iter().any(|item| item.reason == "CHOCH"));
    assert!(signal.evidence.iter().any(|item| item.reason == "BOS"));
    assert!(
        !signal
            .evidence
            .iter()
            .any(|item| item.reason.contains("ORDER_BLOCK"))
    );
}

#[test]
fn real_buy_bos_uses_recent_swing_low_as_invalidation() {
    let release_id = Uuid::now_v7();
    let profile = profile(release_id, HtfFilterMode::Strict);
    let mut snapshot = snapshot(false, Direction::Buy);
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
    ));
    let bos = snapshot
        .target
        .facts
        .iter_mut()
        .filter(|fact| fact.id.kind == FactKind::Structure)
        .find(|fact| {
            let value: StructureEvent = serde_json::from_value(fact.value.clone()).unwrap();
            value.kind == StructureEventKind::BreakOfStructure
        })
        .unwrap();
    let mut value: StructureEvent = serde_json::from_value(bos.value.clone()).unwrap();
    value.broken_swing_id = broken_high_id;
    bos.value = serde_json::to_value(value).unwrap();

    let evaluation = SmcEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = evaluation.signal.unwrap();
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.stop_loss, Decimal::new(4_055, 0));
}

#[test]
fn reversed_structure_order_and_strict_countertrend_are_ineligible() {
    let release_id = Uuid::now_v7();
    let strict = profile(release_id, HtfFilterMode::Strict);
    for snapshot in [
        snapshot(true, Direction::Buy),
        snapshot(false, Direction::Sell),
    ] {
        assert!(
            SmcEvaluator
                .evaluate(&strict, &snapshot, &PlanRuntimeState::neutral(&strict), &[],)
                .unwrap()
                .signal
                .is_none()
        );
    }
}

#[test]
fn weighted_countertrend_applies_versioned_confidence_penalty() {
    let release_id = Uuid::now_v7();
    let weighted = profile(release_id, HtfFilterMode::Weighted);
    let signal = SmcEvaluator
        .evaluate(
            &weighted,
            &snapshot(false, Direction::Sell),
            &PlanRuntimeState::neutral(&weighted),
            &[],
        )
        .unwrap()
        .signal
        .unwrap();
    assert_eq!(signal.confidence, Decimal::new(8, 1));
}

#[test]
fn off_mode_ignores_countertrend_and_minimum_rr_is_enforced() {
    let release_id = Uuid::now_v7();
    let mut disabled = profile(release_id, HtfFilterMode::Off);
    disabled.required_timeframes.clear();
    let signal = SmcEvaluator
        .evaluate(
            &disabled,
            &snapshot(false, Direction::Sell),
            &PlanRuntimeState::neutral(&disabled),
            &[],
        )
        .unwrap()
        .signal
        .unwrap();
    assert_eq!(signal.confidence, Decimal::ONE);

    let mut impossible = disabled;
    let PlanParameters::Smc(parameters) = &mut impossible.parameters else {
        unreachable!()
    };
    parameters.minimum_rr = Decimal::new(100, 0);
    assert!(
        SmcEvaluator
            .evaluate(
                &impossible,
                &snapshot(false, Direction::Sell),
                &PlanRuntimeState::neutral(&impossible),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn off_mode_without_higher_timeframe_can_emit_a_valid_setup() {
    let mut profile = profile(Uuid::now_v7(), HtfFilterMode::Off);
    let PlanParameters::Smc(parameters) = &mut profile.parameters else {
        unreachable!()
    };
    parameters.higher_timeframe = None;
    profile.required_timeframes.clear();
    let mut snapshot = snapshot(false, Direction::Buy);
    snapshot.higher_timeframes.clear();
    let signal = SmcEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
        .expect("valid target setup must not require unavailable HTF facts in off mode");
    assert_eq!(signal.confidence, Decimal::ONE);
}

fn active_fixture() -> (
    MaterializedProfile,
    CanonicalMarketSnapshot,
    PlanRuntimeState,
) {
    let profile = profile(Uuid::now_v7(), HtfFilterMode::Strict);
    let snapshot = snapshot(false, Direction::Buy);
    let evaluation = SmcEvaluator
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
fn expiry_and_swept_target_cancel_unentered_signal() {
    let (profile, mut expired, state) = active_fixture();
    expired.evaluated_closed_candle_time =
        state.active_signal.as_ref().unwrap().expires_at.unwrap();
    let expiry = SmcEvaluator
        .evaluate(&profile, &expired, &state, &[])
        .unwrap();
    assert_eq!(expiry.directives[0].kind, DirectiveKind::ExpireEntry);
    assert_eq!(expiry.events[0].kind, SignalEventKind::Expired);

    let (profile, mut swept, state) = active_fixture();
    swept
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Liquidity)
        .unwrap()
        .lifecycle = FactLifecycle::Swept;
    let invalidated = SmcEvaluator
        .evaluate(&profile, &swept, &state, &[])
        .unwrap();
    assert_eq!(invalidated.directives[0].kind, DirectiveKind::CancelEntry);
    assert_eq!(invalidated.events[0].kind, SignalEventKind::Invalidated);
}

#[test]
fn opposite_structure_cancels_before_entry_and_exits_after_entry() {
    let (profile, mut snapshot, state) = active_fixture();
    let opposite_id = id(FactKind::Structure, Timeframe::M1, 99, 60_000);
    snapshot.target.facts.push(wrapped(
        opposite_id.clone(),
        StructureEvent {
            id: opposite_id,
            kind: StructureEventKind::ChangeOfCharacter,
            direction: Direction::Sell,
            broken_swing_id: id(FactKind::SwingLow, Timeframe::M1, 98, 55_000),
            confirmation_candle_open_time: 60_000,
            confirmation_close: Decimal::new(4_050, 0),
        },
    ));
    snapshot.evaluated_closed_candle_time = 119_999;

    let cancelled = SmcEvaluator
        .evaluate(&profile, &snapshot, &state, &[])
        .unwrap();
    assert_eq!(cancelled.directives[0].kind, DirectiveKind::CancelEntry);

    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let thesis = state
        .active_signal
        .as_ref()
        .unwrap()
        .execution_plan
        .as_ref()
        .unwrap()
        .thesis_invalidation;
    snapshot.target.facts.push(wrapped(
        id(FactKind::MarketClose, Timeframe::M1, 100, 60_001),
        json!({"value": (thesis - Decimal::ONE).to_string()}),
    ));
    let virtual_trade_id = Uuid::now_v7();
    let opened = SmcEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &state,
            &[PlanFeedback::ActualEntry {
                signal_id,
                virtual_trade_id,
                time: 60_000,
                price: Decimal::new(4_105, 0),
            }],
        )
        .unwrap();
    assert_eq!(opened.next_state.trade_state, TradeState::Open);
    assert_eq!(opened.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(opened.directives[0].reason, "SMC_POST_ENTRY_REVERSAL");
    assert!(opened.events.is_empty());
}

#[test]
fn signal_captures_farther_liquidity_as_immutable_extension_target() {
    let profile = profile(Uuid::now_v7(), HtfFilterMode::Strict);
    let mut input = snapshot(false, Direction::Buy);
    let far_id = id(FactKind::Liquidity, Timeframe::M1, 80, 35_001);
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
    ));

    let signal = SmcEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap()
        .signal
        .unwrap();

    assert!(signal.reference_levels.iter().any(|level| {
        level.kind == "TP2_EXTENSION_TARGET" && level.price == Decimal::new(4_300, 0)
    }));
}

#[test]
fn runner_emits_buffered_move_target_from_immutable_smc_liquidity() {
    let profile = profile(Uuid::now_v7(), HtfFilterMode::Strict);
    let mut initial = snapshot(false, Direction::Buy);
    let far_id = id(FactKind::Liquidity, Timeframe::M1, 80, 35_001);
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
    ));
    let created = SmcEvaluator
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
    ));
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 60_000,
            price: Decimal::new(4_105, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 90_000,
            price: Decimal::new(4_162, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let evaluation = SmcEvaluator
        .evaluate(&profile, &progress, &created.next_state, &feedback)
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "SMC_SHARED_TARGET_EXTENSION");
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
fn runner_health_smc_thesis_close_emits_one_emergency_exit() {
    let (profile, mut invalid, state) = active_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let thesis = signal.execution_plan.as_ref().unwrap().thesis_invalidation;
    let trade_id = Uuid::now_v7();
    invalid.target.facts.push(wrapped(
        id(FactKind::MarketClose, Timeframe::M1, 100, 60_000),
        json!({"value": (thesis - Decimal::ONE).to_string()}),
    ));
    invalid.evaluated_closed_candle_time = 119_999;
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id: trade_id,
            time: 60_000,
            price: Decimal::new(4_105, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id: trade_id,
            time: 90_000,
            price: Decimal::new(4_160, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let managed = SmcEvaluator
        .evaluate(&profile, &invalid, &state, &feedback)
        .unwrap();

    assert_eq!(managed.directives.len(), 1);
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(managed.directives[0].reason, "SMC_THESIS_INVALIDATED");
}
