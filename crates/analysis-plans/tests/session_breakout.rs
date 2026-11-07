use analysis_core::SessionRangeFact;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, MaterializedProfile, PlanFeedback, PlanKind, PlanParameters,
    PlanProfileKey, Readiness, ReadinessReason, SessionBreakoutParameters, SignalEvent,
    SignalEventKind, TimeframeFacts, TradeState, VersionIdentity,
};
use analysis_plans::{
    PlanEvaluation, PlanEvaluator, PlanRuntimeState, PreEntryRejection, SessionBreakoutEvaluator,
    apply_terminal_event, validate_session_breakout_entry,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// 2026-03-02 00:00 UTC, a Monday.
const MONDAY: i64 = 1_772_409_600_000;
const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
const MINUTE: i64 = 60_000;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn dec(value: i64, scale: u32) -> Decimal {
    Decimal::new(value, scale)
}

fn parameters() -> SessionBreakoutParameters {
    SessionBreakoutParameters {
        range_start_minute_utc: 0,
        range_end_minute_utc: 420,
        decision_end_minute_utc: 720,
        time_exit_minute_utc: 1200,
        atr_period: 14,
        minimum_normalized_width: dec(5, 1),
        maximum_normalized_width: dec(2, 0),
        target_rr: dec(2, 0),
        minimum_entry_rr: Decimal::ONE,
    }
}

fn profile(timeframe: Timeframe, release_id: Uuid) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::SessionBreakout,
            target_timeframe: timeframe,
        },
        enabled: true,
        versions: VersionIdentity::new(release_id, "orb-v1", "orb-v1-params").unwrap(),
        parameters: PlanParameters::SessionBreakout(parameters()),
        profit_protection: Default::default(),
        position_management: Default::default(),
        required_timeframes: Vec::new(),
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

fn fact_id(timeframe: Timeframe, kind: FactKind, confirmed_at: i64, ordinal: u32) -> FactId {
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

struct Range {
    day_start: i64,
    high: Decimal,
    low: Decimal,
    bar_count: usize,
    width: Option<Decimal>,
}

impl Range {
    fn monday() -> Self {
        Self {
            day_start: MONDAY,
            high: dec(4_110, 0),
            low: dec(4_090, 0),
            bar_count: 28,
            width: Some(dec(95, 2)),
        }
    }
}

fn range_fact(timeframe: Timeframe, range: &Range) -> ConfirmedFact<Value> {
    let expected = usize::try_from(7 * HOUR / timeframe.duration_ms()).unwrap();
    let value = SessionRangeFact {
        session_day: range.day_start / DAY,
        window_start: range.day_start,
        window_end: range.day_start + 7 * HOUR,
        high: range.high,
        low: range.low,
        bar_count: if timeframe == Timeframe::M15 {
            range.bar_count
        } else {
            expected
        },
        expected_bar_count: expected,
        atr_at_range: Some(dec(4, 0)),
        normalized_width: range.width,
    };
    ConfirmedFact {
        id: fact_id(
            timeframe,
            FactKind::SessionRange,
            range.day_start + 7 * HOUR - 1,
            1,
        ),
        value: serde_json::to_value(value).unwrap(),
        lifecycle: FactLifecycle::Confirmed,
    }
}

/// A snapshot for the bar opening at `bar_open` that closed at `close`.
fn snapshot(
    timeframe: Timeframe,
    bar_open: i64,
    close: Decimal,
    ranges: &[Range],
) -> CanonicalMarketSnapshot {
    let evaluated = bar_open + timeframe.duration_ms() - 1;
    let mut facts: Vec<_> = ranges
        .iter()
        .filter(|range| range.day_start + 7 * HOUR - 1 <= evaluated)
        .map(|range| range_fact(timeframe, range))
        .collect();
    facts.push(ConfirmedFact {
        id: fact_id(timeframe, FactKind::MarketClose, evaluated, 2),
        value: json!({"value": close}),
        lifecycle: FactLifecycle::Confirmed,
    });
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: timeframe,
        price_tick: dec(1, 2),
        evaluated_closed_candle_time: evaluated,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe,
            evaluated_closed_candle_time: evaluated,
            facts,
        },
        higher_timeframes: Vec::new(),
        primitive_versions: Vec::new(),
    }
}

fn evaluate(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    state: &PlanRuntimeState,
    feedback: &[PlanFeedback],
) -> PlanEvaluation {
    SessionBreakoutEvaluator
        .evaluate(profile, snapshot, state, feedback)
        .unwrap()
}

#[test]
fn close_above_the_range_gives_a_buy_at_the_next_open() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let bar_open = MONDAY + 7 * HOUR + 15 * MINUTE;
    let snapshot = snapshot(Timeframe::M15, bar_open, dec(4_115, 0), &[Range::monday()]);

    let evaluation = evaluate(&profile, &snapshot, &state, &[]);
    let signal = evaluation.signal.expect("breakout signal");

    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
    assert_eq!(signal.projected_entry, Some(dec(4_115, 0)));
    // Stop at the range midpoint, target at 2R from the breakout close.
    assert_eq!(signal.stop_loss, dec(4_100, 0));
    assert_eq!(signal.primary_target, dec(4_145, 0));
    assert_eq!(signal.risk_reward, dec(2, 0));
    assert_eq!(signal.execution_plan, None);
    assert_eq!(signal.effective_at, bar_open + 15 * MINUTE);
    assert_eq!(signal.expires_at, Some(bar_open + 30 * MINUTE));
    let validation = signal.pre_entry_validation.as_ref().unwrap();
    assert_eq!(validation.minimum_rr, Decimal::ONE);
    assert!(validation.reject_if_stop_crossed && validation.reject_if_target_crossed);
    assert_eq!(evaluation.events[0].kind, SignalEventKind::EntryAvailable);
}

#[test]
fn close_below_the_range_gives_a_sell() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let snapshot = snapshot(
        Timeframe::M15,
        MONDAY + 9 * HOUR,
        dec(4_085, 0),
        &[Range::monday()],
    );

    let signal = evaluate(&profile, &snapshot, &state, &[]).signal.unwrap();
    assert_eq!(signal.direction, Direction::Sell);
    assert_eq!(signal.stop_loss, dec(4_100, 0));
    assert_eq!(signal.primary_target, dec(4_055, 0));
}

#[test]
fn a_close_inside_or_at_the_range_edge_does_nothing() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    for close in [dec(4_110, 0), dec(4_090, 0), dec(4_100, 0)] {
        let snapshot = snapshot(Timeframe::M15, MONDAY + 8 * HOUR, close, &[Range::monday()]);
        assert!(evaluate(&profile, &snapshot, &state, &[]).signal.is_none());
    }
}

#[test]
fn only_bars_opening_in_the_decision_window_can_trigger() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let cases = [
        (MONDAY + 6 * HOUR + 45 * MINUTE, false),
        (MONDAY + 7 * HOUR, true),
        (MONDAY + 11 * HOUR + 45 * MINUTE, true),
        (MONDAY + 12 * HOUR, false),
    ];
    for (bar_open, expected) in cases {
        let snapshot = snapshot(Timeframe::M15, bar_open, dec(4_115, 0), &[Range::monday()]);
        assert_eq!(
            evaluate(&profile, &snapshot, &state, &[]).signal.is_some(),
            expected,
            "bar opening at {bar_open}"
        );
    }
}

#[test]
fn width_filter_keeps_both_bounds_and_rejects_outside() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    for (width, expected) in [
        (Some(dec(49, 2)), false),
        (Some(dec(5, 1)), true),
        (Some(dec(2, 0)), true),
        (Some(dec(201, 2)), false),
        (None, false),
    ] {
        let range = Range {
            width,
            ..Range::monday()
        };
        let snapshot = snapshot(Timeframe::M15, MONDAY + 8 * HOUR, dec(4_115, 0), &[range]);
        assert_eq!(
            evaluate(&profile, &snapshot, &state, &[]).signal.is_some(),
            expected,
            "width {width:?}"
        );
    }
}

#[test]
fn an_incomplete_range_or_another_days_range_gives_no_setup() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let incomplete = Range {
        bar_count: 27,
        ..Range::monday()
    };
    let snapshot_incomplete = snapshot(
        Timeframe::M15,
        MONDAY + 8 * HOUR,
        dec(4_115, 0),
        &[incomplete],
    );
    assert!(
        evaluate(&profile, &snapshot_incomplete, &state, &[])
            .signal
            .is_none()
    );

    // Tuesday's bar must not use Monday's range.
    let snapshot_next_day = snapshot(
        Timeframe::M15,
        MONDAY + DAY + 8 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    assert!(
        evaluate(&profile, &snapshot_next_day, &state, &[])
            .signal
            .is_none()
    );
}

#[test]
fn timeframes_outside_the_plan_stay_silent() {
    for timeframe in [Timeframe::M1, Timeframe::H4, Timeframe::D1] {
        let profile = profile(timeframe, Uuid::nil());
        let state = PlanRuntimeState::neutral(&profile);
        let snapshot = snapshot(
            timeframe,
            MONDAY + 8 * HOUR,
            dec(4_115, 0),
            &[Range::monday()],
        );
        assert!(evaluate(&profile, &snapshot, &state, &[]).signal.is_none());
    }
}

#[test]
fn every_agreed_timeframe_can_trigger() {
    for timeframe in [Timeframe::M5, Timeframe::M15, Timeframe::M30, Timeframe::H1] {
        let profile = profile(timeframe, Uuid::nil());
        let state = PlanRuntimeState::neutral(&profile);
        let snapshot = snapshot(
            timeframe,
            MONDAY + 8 * HOUR,
            dec(4_115, 0),
            &[Range::monday()],
        );
        assert!(evaluate(&profile, &snapshot, &state, &[]).signal.is_some());
    }
}

#[test]
fn not_ready_snapshots_produce_nothing() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let mut snapshot = snapshot(
        Timeframe::M15,
        MONDAY + 8 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    snapshot.readiness = Readiness::NotReady {
        reason: ReadinessReason::UnresolvedGap,
    };
    let evaluation = evaluate(&profile, &snapshot, &state, &[]);
    assert!(evaluation.signal.is_none());
    assert_eq!(evaluation.next_state.readiness, snapshot.readiness);
}

fn expire(state: &mut PlanRuntimeState) {
    let signal = state.active_signal.clone().unwrap();
    apply_terminal_event(
        state,
        &SignalEvent {
            event_id: Uuid::nil(),
            signal_id: signal.signal_id,
            sequence: state.next_event_sequence,
            kind: SignalEventKind::Expired,
            effective_at: signal.effective_at,
            recorded_at: signal.effective_at,
            virtual_trade_id: None,
            price: None,
            reason: None,
        },
    )
    .unwrap();
}

#[test]
fn the_first_breakout_uses_up_the_day_whether_it_fills_or_not() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let first = snapshot(
        Timeframe::M15,
        MONDAY + 7 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    let mut state =
        evaluate(&profile, &first, &PlanRuntimeState::neutral(&profile), &[]).next_state;

    // Cancelled or expired before the fill: still no second attempt that day.
    let mut cancelled = state.clone();
    expire(&mut cancelled);
    assert!(cancelled.active_signal.is_none());
    let later = snapshot(
        Timeframe::M15,
        MONDAY + 9 * HOUR,
        dec(4_080, 0),
        &[Range::monday()],
    );
    assert!(evaluate(&profile, &later, &cancelled, &[]).signal.is_none());

    // Filled and closed: no second trade that day either.
    let signal = state.active_signal.clone().unwrap();
    let trade_id = Uuid::from_u128(7);
    let opened = evaluate(
        &profile,
        &snapshot(
            Timeframe::M15,
            MONDAY + 7 * HOUR + 15 * MINUTE,
            dec(4_116, 0),
            &[Range::monday()],
        ),
        &state,
        &[PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: signal.effective_at,
            price: dec(4_116, 0),
        }],
    );
    state = opened.next_state;
    assert_eq!(state.trade_state, TradeState::Open);
    let closed = evaluate(
        &profile,
        &later,
        &state,
        &[PlanFeedback::TradeClosed {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: MONDAY + 9 * HOUR,
            reason: "STOP_LOSS".to_owned(),
        }],
    );
    assert!(closed.signal.is_none());
    let after = evaluate(
        &profile,
        &snapshot(
            Timeframe::M15,
            MONDAY + 10 * HOUR,
            dec(4_080, 0),
            &[Range::monday()],
        ),
        &closed.next_state,
        &[],
    );
    assert!(after.signal.is_none());

    // The next day is a new day.
    let tuesday = Range {
        day_start: MONDAY + DAY,
        ..Range::monday()
    };
    let next_day = snapshot(
        Timeframe::M15,
        MONDAY + DAY + 8 * HOUR,
        dec(4_115, 0),
        &[Range::monday(), tuesday],
    );
    assert!(
        evaluate(&profile, &next_day, &after.next_state, &[])
            .signal
            .is_some()
    );
}

#[test]
fn an_open_trade_leaves_at_the_twenty_hundred_open() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let first = snapshot(
        Timeframe::M15,
        MONDAY + 7 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    let state = evaluate(&profile, &first, &PlanRuntimeState::neutral(&profile), &[]).next_state;
    let signal = state.active_signal.clone().unwrap();
    let opened = evaluate(
        &profile,
        &snapshot(
            Timeframe::M15,
            MONDAY + 7 * HOUR + 15 * MINUTE,
            dec(4_116, 0),
            &[Range::monday()],
        ),
        &state,
        &[PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: Uuid::from_u128(9),
            time: signal.effective_at,
            price: dec(4_116, 0),
        }],
    )
    .next_state;

    let before = snapshot(
        Timeframe::M15,
        MONDAY + 19 * HOUR + 30 * MINUTE,
        dec(4_120, 0),
        &[Range::monday()],
    );
    assert!(
        evaluate(&profile, &before, &opened, &[])
            .directives
            .is_empty()
    );

    let last = snapshot(
        Timeframe::M15,
        MONDAY + 19 * HOUR + 45 * MINUTE,
        dec(4_120, 0),
        &[Range::monday()],
    );
    let evaluation = evaluate(&profile, &last, &opened, &[]);
    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::ExitAtNextOpen);
    assert_eq!(directive.reason, "ORB_TIME_EXIT");
    assert_eq!(directive.effective_at, MONDAY + 20 * HOUR);
}

#[test]
fn signal_ids_are_deterministic() {
    let profile = profile(Timeframe::M15, Uuid::from_u128(42));
    let state = PlanRuntimeState::neutral(&profile);
    let snapshot = snapshot(
        Timeframe::M15,
        MONDAY + 8 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    let first = evaluate(&profile, &snapshot, &state, &[]).signal.unwrap();
    let second = evaluate(&profile, &snapshot, &state, &[]).signal.unwrap();
    assert_eq!(first.signal_id, second.signal_id);
}

#[test]
fn next_open_validation_cancels_gaps_and_poor_reward() {
    let profile = profile(Timeframe::M15, Uuid::nil());
    let state = PlanRuntimeState::neutral(&profile);
    let snapshot = snapshot(
        Timeframe::M15,
        MONDAY + 8 * HOUR,
        dec(4_115, 0),
        &[Range::monday()],
    );
    let signal = evaluate(&profile, &snapshot, &state, &[]).signal.unwrap();

    // Stop 4100, target 4145.
    assert_eq!(
        validate_session_breakout_entry(&signal, dec(4_115, 0)),
        Ok(dec(2, 0))
    );
    assert_eq!(
        validate_session_breakout_entry(&signal, dec(4_100, 0)),
        Err(PreEntryRejection::StopCrossed)
    );
    assert_eq!(
        validate_session_breakout_entry(&signal, dec(4_145, 0)),
        Err(PreEntryRejection::TargetCrossed)
    );
    // At 4125: reward 20, risk 25, below the 1.0 minimum.
    assert_eq!(
        validate_session_breakout_entry(&signal, dec(4_125, 0)),
        Err(PreEntryRejection::InsufficientRiskReward)
    );
    let mut other = signal.clone();
    other.plan = PlanKind::MeanReversion;
    assert_eq!(
        validate_session_breakout_entry(&other, dec(4_115, 0)),
        Err(PreEntryRejection::WrongPlan)
    );
}
