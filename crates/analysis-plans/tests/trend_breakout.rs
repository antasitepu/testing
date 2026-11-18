use analysis_core::{ChandelierFact, ConfirmedSwing, DonchianFact, SwingKind};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HtfAlignment, PlanFeedback, PlanKind, PlanParameters, PlanProfileKey,
    PrimitiveVersion, Readiness, SignalDefinition, TimeframeFacts, TradeState,
    TrendBreakoutParameters, VersionIdentity,
};
use analysis_plans::{PlanEvaluator, PlanRuntimeState, TrendBreakoutEvaluator};
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

fn fact<T: Serialize>(id: FactId, value: T) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id,
        value: serde_json::to_value(value).unwrap(),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn profile() -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::TrendBreakout,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(Uuid::now_v7(), "trend-v1", "trend-1m-v1").unwrap(),
        parameters: PlanParameters::TrendBreakout(TrendBreakoutParameters {
            donchian_period: 20,
            ema_period: 20,
            adx_period: 14,
            minimum_adx: Decimal::new(25, 0),
            higher_timeframe: Some(Timeframe::M5),
            atr_period: 14,
            initial_stop_atr_multiplier: Decimal::TWO,
            target_rr: Decimal::TWO,
            trailing_activation_r: Decimal::ONE,
            chandelier_period: 20,
            chandelier_atr_multiplier: Decimal::new(3, 0),
        }),
        required_timeframes: vec![Timeframe::M5],
        minimum_history_bars: 100,
        warmup_bars: 30,
    }
}

fn snapshot(direction: Direction, close: Decimal, adx: Decimal) -> CanonicalMarketSnapshot {
    let slope = if direction == Direction::Buy {
        Decimal::new(2, 0)
    } else {
        Decimal::new(-2, 0)
    };
    let donchian_id = id(FactKind::Donchian, Timeframe::M1, 1, 180_000);
    let ema_id = id(FactKind::Ema, Timeframe::M1, 2, 180_000);
    let adx_id = id(FactKind::Adx, Timeframe::M1, 3, 180_000);
    let atr_id = id(FactKind::Atr, Timeframe::M1, 4, 180_000);
    let close_id = id(FactKind::MarketClose, Timeframe::M1, 5, 180_000);
    let htf_id = id(FactKind::Ema, Timeframe::M5, 1, 180_000);
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: 239_999,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe: Timeframe::M1,
            evaluated_closed_candle_time: 239_999,
            facts: vec![
                fact(
                    donchian_id,
                    json!({
                        "period": 20,
                        "upper": "4090",
                        "lower": "4010",
                        "sourceEndTime": 179999,
                        "close": close.to_string()
                    }),
                ),
                fact(ema_id, json!({"value": "4050", "slope": slope.to_string()})),
                fact(adx_id, json!({"value": adx.to_string()})),
                fact(atr_id, json!({"value": "10"})),
                fact(close_id, json!({"value": close.to_string()})),
            ],
        },
        higher_timeframes: vec![TimeframeFacts {
            timeframe: Timeframe::M5,
            evaluated_closed_candle_time: 239_999,
            facts: vec![fact(
                htf_id,
                json!({"value": "4040", "slope": slope.to_string()}),
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
fn previous_channel_close_breakout_emits_symmetric_next_open_signal() {
    for (direction, close) in [
        (Direction::Buy, Decimal::new(4_100, 0)),
        (Direction::Sell, Decimal::new(4_000, 0)),
    ] {
        let profile = profile();
        let signal = TrendBreakoutEvaluator
            .evaluate(
                &profile,
                &snapshot(direction, close, Decimal::new(30, 0)),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_eq!(signal.direction, direction);
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
        assert_eq!(signal.projected_entry, Some(close));
        let actual_rr =
            ((signal.primary_target - close).abs() / (close - signal.stop_loss).abs()).round_dp(6);
        assert_eq!(signal.risk_reward, actual_rr);
        assert_eq!(signal.effective_at, 240_000);
    }
}

#[test]
fn execution_plan_uses_initial_donchian_breakout_level_symmetrically() {
    for (direction, close, thesis) in [
        (
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_090, 0),
        ),
        (
            Direction::Sell,
            Decimal::new(4_000, 0),
            Decimal::new(4_010, 0),
        ),
    ] {
        let profile = profile();
        let signal = TrendBreakoutEvaluator
            .evaluate(
                &profile,
                &snapshot(direction, close, Decimal::new(30, 0)),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_execution_plan(&signal, thesis);
        assert_eq!(
            signal
                .reference_levels
                .iter()
                .find(|level| level.kind == "BREAKOUT_LEVEL")
                .map(|level| level.price),
            Some(thesis)
        );
    }
}

#[test]
fn signal_captures_farther_structural_reference_as_immutable_extension_target() {
    let profile = profile();
    let mut input = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let far = ConfirmedSwing {
        id: id(FactKind::SwingHigh, Timeframe::M1, 80, 180_001),
        kind: SwingKind::High,
        label: None,
        price: Decimal::new(4_250, 0),
        pivot_open_time: 160_000,
        confirmed_at: 180_001,
        source_start_time: 150_000,
        source_end_time: 180_001,
    };
    input.target.facts.push(fact(far.id.clone(), far));

    let signal = TrendBreakoutEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap()
        .signal
        .unwrap();

    assert!(signal.reference_levels.iter().any(|level| {
        level.kind == "TP2_EXTENSION_TARGET" && level.price == Decimal::new(4_250, 0)
    }));
}

#[test]
fn runner_emits_buffered_move_target_from_immutable_trend_reference() {
    let profile = profile();
    let mut initial = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let far = ConfirmedSwing {
        id: id(FactKind::SwingHigh, Timeframe::M1, 80, 180_001),
        kind: SwingKind::High,
        label: None,
        price: Decimal::new(4_250, 0),
        pivot_open_time: 160_000,
        confirmed_at: 180_001,
        source_start_time: 150_000,
        source_end_time: 180_001,
    };
    initial.target.facts.push(fact(far.id.clone(), far));
    let created = TrendBreakoutEvaluator
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
    let progress = with_chandelier(initial, Decimal::new(4_135, 0), Decimal::new(4_100, 0));

    let evaluation = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &progress,
            &created.next_state,
            &runner_feedback(signal.signal_id, trade_id, 240_000, Decimal::new(4_100, 0)),
        )
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "TREND_SHARED_TARGET_EXTENSION");
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
fn ema_adx_and_htf_filters_block_ineligible_breakouts() {
    let profile = profile();
    let mut wrong_ema = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    wrong_ema
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Ema)
        .unwrap()
        .value["slope"] = json!("-1");
    let weak_adx = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(20, 0));
    let mut wrong_htf = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    wrong_htf.higher_timeframes[0].facts[0].value["slope"] = json!("-1");
    for invalid in [wrong_ema, weak_adx, wrong_htf] {
        assert!(
            TrendBreakoutEvaluator
                .evaluate(
                    &profile,
                    &invalid,
                    &PlanRuntimeState::neutral(&profile),
                    &[],
                )
                .unwrap()
                .signal
                .is_none()
        );
    }
}

fn opened_fixture() -> (
    MaterializedProfile,
    CanonicalMarketSnapshot,
    PlanRuntimeState,
    Uuid,
) {
    let profile = profile();
    let snapshot = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let created = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let trade_id = Uuid::now_v7();
    (profile, snapshot, state, trade_id)
}

fn runner_feedback(
    signal_id: Uuid,
    virtual_trade_id: Uuid,
    time: i64,
    entry: Decimal,
) -> [PlanFeedback; 2] {
    [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time,
            price: entry,
        },
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            time: time + 1,
            price: entry,
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ]
}

fn with_chandelier(
    mut snapshot: CanonicalMarketSnapshot,
    close: Decimal,
    candidate: Decimal,
) -> CanonicalMarketSnapshot {
    snapshot.evaluated_closed_candle_time += 60_000;
    snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Donchian)
        .unwrap()
        .value["close"] = json!(close.to_string());
    snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .unwrap()
        .value["value"] = json!(close.to_string());
    let chandelier_id = id(
        FactKind::Chandelier,
        Timeframe::M1,
        9,
        snapshot.evaluated_closed_candle_time,
    );
    snapshot.target.facts.push(fact(
        chandelier_id,
        ChandelierFact {
            period: 20,
            atr_multiplier: Decimal::new(3, 0),
            long_candidate: candidate,
            short_candidate: Decimal::new(4_150, 0),
            source_end_time: snapshot.evaluated_closed_candle_time,
            effective_at: snapshot.evaluated_closed_candle_time + 1,
        },
    ));
    snapshot
}

#[test]
fn trailing_activates_at_one_r_and_never_moves_away_from_profit() {
    let (profile, snapshot, state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let entry = Decimal::new(4_100, 0);
    let risk = entry - signal.stop_loss;
    let first = with_chandelier(
        snapshot.clone(),
        entry + risk + Decimal::new(5, 0),
        Decimal::new(4_110, 0),
    );
    let moved = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &first,
            &state,
            &runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_100, 0)),
        )
        .unwrap();
    assert_eq!(moved.directives[0].kind, DirectiveKind::MoveStop);
    assert_eq!(moved.directives[0].price, Some(Decimal::new(4_110, 0)));

    let worse = with_chandelier(first, Decimal::new(4_129, 0), Decimal::new(4_105, 0));
    let held = TrendBreakoutEvaluator
        .evaluate(&profile, &worse, &moved.next_state, &[])
        .unwrap();
    assert!(held.directives.is_empty());
}

#[test]
fn counter_htf_runner_uses_more_protective_half_r_step() {
    let (profile, snapshot, mut state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_mut().unwrap();
    signal.htf_context.as_mut().unwrap().alignment = HtfAlignment::Counter;
    let signal_id = signal.signal_id;
    let entry = Decimal::new(4_100, 0);
    let risk = entry - signal.stop_loss;
    let first = with_chandelier(
        snapshot,
        entry + risk / Decimal::TWO + Decimal::ONE,
        Decimal::new(4_105, 0),
    );

    let moved = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &first,
            &state,
            &runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_100, 0)),
        )
        .unwrap();

    assert_eq!(moved.directives[0].kind, DirectiveKind::MoveStop);
    assert_eq!(moved.directives[0].price, Some(entry + risk / Decimal::TWO));
    assert_eq!(moved.directives[0].reason, "TREND_STEPPED_PROFIT_LOCK");
}

#[test]
fn stepped_stop_wins_when_more_protective_than_chandelier() {
    let (profile, snapshot, state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let entry = Decimal::new(4_100, 0);
    let risk = entry - signal.stop_loss;
    let at_one_and_half_r = with_chandelier(
        snapshot,
        entry + risk * Decimal::new(15, 1),
        Decimal::new(4_105, 0),
    );

    let moved = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &at_one_and_half_r,
            &state,
            &runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_100, 0)),
        )
        .unwrap();

    assert_eq!(moved.directives[0].kind, DirectiveKind::MoveStop);
    assert_eq!(moved.directives[0].price, Some(entry + risk / Decimal::TWO));
    assert_eq!(moved.directives[0].reason, "TREND_STEPPED_PROFIT_LOCK");
}

#[test]
fn profitable_trend_reversal_suggests_close_after_two_adverse_closes() {
    let (profile, snapshot, state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let entry = Decimal::new(4_100, 0);
    let risk = entry - signal.stop_loss;
    let peak = with_chandelier(snapshot.clone(), entry + risk, Decimal::new(4_100, 0));
    let protected = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &peak,
            &state,
            &runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_100, 0)),
        )
        .unwrap();

    let first_adverse = with_chandelier(
        peak.clone(),
        entry + risk * Decimal::new(8, 1),
        Decimal::new(4_100, 0),
    );
    let watching = TrendBreakoutEvaluator
        .evaluate(&profile, &first_adverse, &protected.next_state, &[])
        .unwrap();
    assert!(watching.directives.is_empty());

    let second_adverse = with_chandelier(
        first_adverse,
        entry + risk * Decimal::new(4, 1),
        Decimal::new(4_100, 0),
    );
    let exited = TrendBreakoutEvaluator
        .evaluate(&profile, &second_adverse, &watching.next_state, &[])
        .unwrap();

    assert_eq!(exited.directives[0].kind, DirectiveKind::SuggestClose);
    assert_eq!(exited.directives[0].reason, "TREND_PROFIT_REVERSAL");
}

#[test]
fn execution_plan_trend_does_not_extend_target_without_plan_supplied_level() {
    let (profile, snapshot, state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_ref().unwrap().clone();
    let signal_id = signal.signal_id;
    let entry = Decimal::new(4_100, 0);
    let initial_risk = (entry - signal.stop_loss).abs();
    let target_distance = (signal.primary_target - entry).abs();
    let first = with_chandelier(
        snapshot.clone(),
        entry + initial_risk + Decimal::ONE,
        Decimal::new(4_110, 0),
    );
    let moved_stop = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &first,
            &state,
            &runner_feedback(signal_id, trade_id, 240_000, entry),
        )
        .unwrap();
    assert_eq!(moved_stop.directives[0].kind, DirectiveKind::MoveStop);

    let near_target = with_chandelier(
        snapshot,
        entry + target_distance * Decimal::new(8, 1),
        Decimal::new(4_110, 0),
    );
    let extended = TrendBreakoutEvaluator
        .evaluate(&profile, &near_target, &moved_stop.next_state, &[])
        .unwrap();

    assert!(extended.directives.is_empty());
}

#[test]
fn opposite_close_breakout_reverses_only_when_all_filters_pass() {
    let (profile, _, state, trade_id) = opened_fixture();
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let feedback = [PlanFeedback::ActualEntry {
        signal_id,
        virtual_trade_id: trade_id,
        time: 240_000,
        price: Decimal::new(4_100, 0),
    }];
    let valid = snapshot(Direction::Sell, Decimal::new(4_000, 0), Decimal::new(30, 0));
    let reversed = TrendBreakoutEvaluator
        .evaluate(&profile, &valid, &state, &feedback)
        .unwrap();
    assert_eq!(
        reversed.directives[0].kind,
        DirectiveKind::ReverseAtNextOpen
    );
    assert_eq!(
        reversed.directives[0].reason,
        "TREND_VALID_OPPOSITE_BREAKOUT"
    );

    let invalid = snapshot(Direction::Sell, Decimal::new(4_000, 0), Decimal::new(20, 0));
    let exited = TrendBreakoutEvaluator
        .evaluate(&profile, &invalid, &state, &feedback)
        .unwrap();
    assert_eq!(exited.directives[0].kind, DirectiveKind::EmergencyExit);
}

#[test]
fn runner_health_trend_thesis_close_emits_one_emergency_exit_without_opposite_breakout() {
    let (profile, mut invalid, state, trade_id) = opened_fixture();
    let signal = state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let thesis = signal.execution_plan.as_ref().unwrap().thesis_invalidation;
    invalid
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::MarketClose)
        .unwrap()
        .value = json!({"value": (thesis - Decimal::ONE).to_string()});
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
            price: Decimal::new(4_130, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let managed = TrendBreakoutEvaluator
        .evaluate(&profile, &invalid, &state, &feedback)
        .unwrap();

    assert_eq!(managed.directives.len(), 1);
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(managed.directives[0].reason, "TREND_THESIS_INVALIDATED");
}

#[test]
fn closed_trade_feedback_releases_active_slot_immediately() {
    let (profile, _, state, trade_id) = opened_fixture();
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let closed = [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id: trade_id,
            time: 240_000,
            price: Decimal::new(4_100, 0),
        },
        PlanFeedback::TradeClosed {
            signal_id,
            virtual_trade_id: trade_id,
            time: 260_000,
            reason: "TARGET".to_owned(),
        },
    ];
    let still_outside = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let released = TrendBreakoutEvaluator
        .evaluate(&profile, &still_outside, &state, &closed)
        .unwrap();
    assert!(released.signal.is_none());
    assert!(released.next_state.active_signal.is_none());
    assert!(released.next_state.active_virtual_trade_id.is_none());
    assert_eq!(released.next_state.direction, Direction::None);
    assert_eq!(released.next_state.trade_state, TradeState::Closed);

    let breakout = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    assert!(
        TrendBreakoutEvaluator
            .evaluate(&profile, &breakout, &released.next_state, &[])
            .unwrap()
            .signal
            .is_some()
    );
}

#[test]
fn micro_breakout_inside_atr_margin_is_rejected() {
    let profile = profile();
    // Band upper=4090, ATR=10 → need >1.0 beyond band; 4090.5 is noise.
    let snap = snapshot(Direction::Buy, Decimal::new(40_905, 1), Decimal::new(30, 0));
    let evaluation = TrendBreakoutEvaluator
        .evaluate(&profile, &snap, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap();
    assert!(evaluation.signal.is_none());
}

#[test]
fn not_entered_signal_cancels_when_channel_is_reclaimed() {
    let profile = profile();
    let breakout = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let armed = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &breakout,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    assert!(armed.signal.is_some());
    assert_eq!(armed.next_state.trade_state, TradeState::NotEntered);

    let reclaimed = snapshot(Direction::Buy, Decimal::new(4_080, 0), Decimal::new(30, 0));
    let cancelled = TrendBreakoutEvaluator
        .evaluate(&profile, &reclaimed, &armed.next_state, &[])
        .unwrap();
    assert_eq!(cancelled.directives[0].kind, DirectiveKind::CancelEntry);
    assert_eq!(
        cancelled.directives[0].reason,
        "TREND_FALSE_BREAKOUT_RECLAIM"
    );
}

#[test]
fn not_entered_signal_expires_after_next_open_window() {
    let profile = profile();
    let breakout = snapshot(Direction::Buy, Decimal::new(4_100, 0), Decimal::new(30, 0));
    let armed = TrendBreakoutEvaluator
        .evaluate(
            &profile,
            &breakout,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let mut still_outside = snapshot(Direction::Buy, Decimal::new(4_110, 0), Decimal::new(30, 0));
    still_outside.evaluated_closed_candle_time = 299_999;
    still_outside.target.evaluated_closed_candle_time = 299_999;
    let expired = TrendBreakoutEvaluator
        .evaluate(&profile, &still_outside, &armed.next_state, &[])
        .unwrap();
    assert_eq!(expired.directives[0].kind, DirectiveKind::ExpireEntry);
    assert_eq!(expired.directives[0].reason, "TREND_NEXT_OPEN_MISSED");
}

#[test]
fn donchian_fixture_is_previous_window_data() {
    let source = DonchianFact {
        period: 20,
        upper: Decimal::new(4_090, 0),
        lower: Decimal::new(4_010, 0),
        source_end_time: 179_999,
    };
    assert!(source.source_end_time < 180_000);
}
