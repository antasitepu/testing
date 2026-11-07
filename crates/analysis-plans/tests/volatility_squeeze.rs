use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, MaterializedProfile, PlanFeedback, PlanKind, PlanParameters,
    PlanProfileKey, Readiness, SignalEvent, SignalEventKind, TimeframeFacts, TradeState,
    VersionIdentity, VolatilitySqueezeParameters,
};
use analysis_plans::{
    PlanEvaluation, PlanEvaluator, PlanRuntimeState, VolatilitySqueezeEvaluator,
    apply_terminal_event,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

const START: i64 = 1_772_409_600_000;
const STEP: i64 = 300_000;
const ATR: i64 = 4;
const MIDDLE: i64 = 4_100;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn dec(value: i64, scale: u32) -> Decimal {
    Decimal::new(value, scale)
}

fn parameters() -> VolatilitySqueezeParameters {
    VolatilitySqueezeParameters {
        bollinger_period: 20,
        bollinger_deviation: dec(2, 0),
        keltner_atr_multiplier: dec(15, 1),
        atr_period: 14,
        minimum_squeeze_bars: 6,
        breakout_window_bars: 3,
        breakout_margin_atr: dec(1, 1),
        stop_buffer_atr: dec(5, 1),
        target_rr: dec(2, 0),
        max_bars_to_tp1: 20,
    }
}

fn profile() -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::VolatilitySqueeze,
            target_timeframe: Timeframe::M5,
        },
        enabled: true,
        versions: VersionIdentity::new(Uuid::nil(), "squeeze-v1", "squeeze-5m-v1").unwrap(),
        parameters: PlanParameters::VolatilitySqueeze(parameters()),
        profit_protection: Default::default(),
        position_management: Default::default(),
        required_timeframes: Vec::new(),
        minimum_history_bars: 200,
        warmup_bars: 50,
    }
}

#[derive(Clone, Copy)]
enum Kind {
    /// Wide bands: deviation 4 with ATR 4, squeeze off.
    Wide,
    /// Narrow bands: deviation 2, squeeze on.
    Narrow,
    /// Deviation exactly 0.75 ATR: squeeze off (strict comparison).
    Boundary,
    /// Wide bands with a close 0.5 above the upper band (>= 0.1 ATR).
    BreakUp,
    /// Wide bands with a close 0.5 below the lower band.
    BreakDown,
    /// Wide bands with a close only 0.3 above the upper band (< 0.1 ATR).
    Weak,
    /// Wide bands, close below the middle.
    BelowMiddle,
}

#[derive(Clone, Copy)]
struct Bar {
    time: i64,
    kind: Kind,
}

/// Bars one step apart; `gaps` lists bar indices preceded by a closure jump.
fn sequence(kinds: &[Kind], gaps: &[usize]) -> Vec<Bar> {
    let mut time = START;
    kinds
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            if index > 0 {
                time += if gaps.contains(&index) {
                    48 * 3_600_000
                } else {
                    STEP
                };
            }
            Bar { time, kind: *kind }
        })
        .collect()
}

fn band(bar: &Bar, ordinal: u32) -> ConfirmedFact<Value> {
    let deviation = match bar.kind {
        Kind::Narrow => dec(2, 0),
        Kind::Boundary => dec(3, 0),
        _ => dec(4, 0),
    };
    let middle = dec(MIDDLE, 0);
    let upper = middle + deviation * dec(2, 0);
    let lower = middle - deviation * dec(2, 0);
    let close = match bar.kind {
        Kind::BreakUp => upper + dec(5, 1),
        Kind::BreakDown => lower - dec(5, 1),
        Kind::Weak => upper + dec(3, 1),
        Kind::BelowMiddle => middle - dec(1, 0),
        _ => middle,
    };
    ConfirmedFact {
        id: id(FactKind::Bollinger, bar.time + STEP - 1, ordinal),
        value: json!({
            "period": 20,
            "deviationMultiplier": "2",
            "upper": upper,
            "middle": middle,
            "lower": lower,
            "deviation": deviation,
            "bandwidth": "0.01",
            "zscore": "0",
            "close": close,
            "low": close,
            "high": close
        }),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn id(kind: FactKind, confirmed_at: i64, ordinal: u32) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M5,
        kind,
        confirmed_at,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    }
}

fn snapshot(bars: &[Bar]) -> CanonicalMarketSnapshot {
    let mut facts = Vec::new();
    for (index, bar) in bars.iter().enumerate() {
        let ordinal = u32::try_from(index).unwrap();
        facts.push(band(bar, ordinal));
        facts.push(ConfirmedFact {
            id: id(FactKind::Atr, bar.time + STEP - 1, ordinal),
            value: json!({"value": dec(ATR, 0)}),
            lifecycle: FactLifecycle::Confirmed,
        });
    }
    let last = bars.last().unwrap();
    let close = band(last, 0).value["close"].clone();
    let evaluated = last.time + STEP - 1;
    facts.push(ConfirmedFact {
        id: id(FactKind::MarketClose, evaluated, 0),
        value: json!({"value": close}),
        lifecycle: FactLifecycle::Confirmed,
    });
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M5,
        price_tick: dec(1, 2),
        evaluated_closed_candle_time: evaluated,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe: Timeframe::M5,
            evaluated_closed_candle_time: evaluated,
            facts,
        },
        higher_timeframes: Vec::new(),
        primitive_versions: Vec::new(),
    }
}

fn evaluate(
    snapshot: &CanonicalMarketSnapshot,
    state: &PlanRuntimeState,
    feedback: &[PlanFeedback],
) -> PlanEvaluation {
    VolatilitySqueezeEvaluator
        .evaluate(&profile(), snapshot, state, feedback)
        .unwrap()
}

fn neutral() -> PlanRuntimeState {
    PlanRuntimeState::neutral(&profile())
}

/// 30 wide bars, `squeeze` narrow bars, then `tail`.
fn history(squeeze: usize, tail: &[Kind]) -> Vec<Kind> {
    let mut kinds = vec![Kind::Wide; 30];
    kinds.extend(std::iter::repeat_n(Kind::Narrow, squeeze));
    kinds.extend_from_slice(tail);
    kinds
}

fn signal_for(kinds: &[Kind], gaps: &[usize]) -> Option<analysis_domain::SignalDefinition> {
    evaluate(&snapshot(&sequence(kinds, gaps)), &neutral(), &[]).signal
}

#[test]
fn six_squeeze_bars_arm_and_a_release_breakout_gives_a_buy() {
    let kinds = history(6, &[Kind::BreakUp]);
    let bars = sequence(&kinds, &[]);
    let signal = evaluate(&snapshot(&bars), &neutral(), &[]).signal.unwrap();

    let close = dec(MIDDLE + 8, 0) + dec(5, 1);
    assert_eq!(signal.direction, Direction::Buy);
    assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
    assert_eq!(signal.projected_entry, Some(close));
    let plan = signal.execution_plan.as_ref().unwrap();
    // Thesis is the frozen middle; hard stop 0.5 ATR (2) beyond it; 2R target.
    assert_eq!(plan.thesis_invalidation, dec(MIDDLE, 0));
    assert_eq!(signal.stop_loss, dec(MIDDLE - 2, 0));
    let risk = close - signal.stop_loss;
    assert_eq!(signal.primary_target, close + risk * dec(2, 0));
    let evaluated = bars.last().unwrap().time + STEP - 1;
    assert_eq!(signal.effective_at, evaluated + 1);
    assert_eq!(signal.expires_at, Some(evaluated + 1 + STEP));
}

#[test]
fn sell_breakout_mirrors_the_buy() {
    let signal = signal_for(&history(6, &[Kind::BreakDown]), &[]).unwrap();
    assert_eq!(signal.direction, Direction::Sell);
    assert_eq!(signal.stop_loss, dec(MIDDLE + 2, 0));
}

#[test]
fn five_squeeze_bars_never_arm() {
    assert!(signal_for(&history(5, &[Kind::BreakUp]), &[]).is_none());
}

#[test]
fn deviation_at_exactly_three_quarters_of_atr_is_not_a_squeeze() {
    let mut kinds = vec![Kind::Wide; 30];
    kinds.extend(std::iter::repeat_n(Kind::Boundary, 8));
    kinds.push(Kind::BreakUp);
    assert!(signal_for(&kinds, &[]).is_none());
}

#[test]
fn breakout_may_come_up_to_two_bars_after_the_release() {
    assert!(signal_for(&history(6, &[Kind::Wide, Kind::BreakUp]), &[]).is_some());
    assert!(signal_for(&history(6, &[Kind::Wide, Kind::Wide, Kind::BreakUp]), &[]).is_some());
    assert!(
        signal_for(
            &history(6, &[Kind::Wide, Kind::Wide, Kind::Wide, Kind::BreakUp]),
            &[]
        )
        .is_none()
    );
}

#[test]
fn only_the_first_decisive_close_after_the_release_counts() {
    // A weak close outside the band is not a breakout; a later decisive one is.
    assert!(signal_for(&history(6, &[Kind::Weak, Kind::BreakUp]), &[]).is_some());
    // After a decisive breakout, a second one is not a new signal.
    assert!(signal_for(&history(6, &[Kind::BreakUp, Kind::BreakUp]), &[]).is_none());
}

#[test]
fn squeeze_returning_before_the_breakout_cancels_the_release() {
    let kinds = history(6, &[Kind::Wide, Kind::Narrow, Kind::BreakUp]);
    assert!(signal_for(&kinds, &[]).is_none());
}

#[test]
fn a_closure_jump_resets_the_run_and_blocks_releases_after_a_reopen() {
    // Jump inside the run: the bars after it are not closure-clean for 20 bars.
    let kinds = history(6, &[Kind::BreakUp]);
    assert!(signal_for(&kinds, &[33]).is_none());
    // Reopen right before the release: the release bar is not closure-clean.
    assert!(signal_for(&kinds, &[36]).is_none());
    // A jump far enough back (more than 20 bars before the run) does not matter.
    assert!(signal_for(&kinds, &[5]).is_some());
}

#[test]
fn a_mismatched_bollinger_period_gives_no_setup() {
    let bars = sequence(&history(6, &[Kind::BreakUp]), &[]);
    let mut snapshot = snapshot(&bars);
    for fact in &mut snapshot.target.facts {
        if fact.id.kind == FactKind::Bollinger {
            fact.value["period"] = json!(21);
        }
    }
    assert!(evaluate(&snapshot, &neutral(), &[]).signal.is_none());
}

fn opened() -> (
    PlanRuntimeState,
    Vec<Kind>,
    analysis_domain::SignalDefinition,
) {
    let kinds = history(6, &[Kind::BreakUp]);
    let state = evaluate(&snapshot(&sequence(&kinds, &[])), &neutral(), &[]).next_state;
    let signal = state.active_signal.clone().unwrap();
    let mut next = kinds.clone();
    next.push(Kind::Wide);
    let state = evaluate(
        &snapshot(&sequence(&next, &[])),
        &state,
        &[PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: Uuid::from_u128(5),
            time: signal.effective_at,
            price: signal.projected_entry.unwrap(),
        }],
    )
    .next_state;
    assert_eq!(state.trade_state, TradeState::Open);
    (state, next, signal)
}

#[test]
fn no_tp1_after_twenty_bars_exits_at_the_next_open() {
    let (state, mut kinds, _) = opened();
    // One bar after entry is already in `kinds`; add 18 more: 19 bars since entry.
    kinds.extend(std::iter::repeat_n(Kind::Wide, 18));
    let nineteen = evaluate(&snapshot(&sequence(&kinds, &[])), &state, &[]);
    assert!(nineteen.directives.is_empty());

    kinds.push(Kind::Wide);
    let twenty = evaluate(&snapshot(&sequence(&kinds, &[])), &state, &[]);
    assert_eq!(twenty.directives.len(), 1);
    assert_eq!(twenty.directives[0].kind, DirectiveKind::ExitAtNextOpen);
    assert_eq!(twenty.directives[0].reason, "SQUEEZE_NO_EXPANSION");

    // After TP1 there is no time exit.
    let mut runner = state.clone();
    runner.tp1_filled_at = Some(START);
    assert!(
        evaluate(&snapshot(&sequence(&kinds, &[])), &runner, &[])
            .directives
            .iter()
            .all(|directive| directive.kind != DirectiveKind::ExitAtNextOpen)
    );
}

#[test]
fn a_close_back_below_the_middle_invalidates_the_thesis() {
    let (state, mut kinds, _) = opened();
    kinds.push(Kind::BelowMiddle);
    let evaluation = evaluate(&snapshot(&sequence(&kinds, &[])), &state, &[]);
    assert_eq!(evaluation.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(
        evaluation.directives[0].reason,
        "SQUEEZE_THESIS_INVALIDATED"
    );
}

#[test]
fn an_unfilled_signal_expires_on_the_next_evaluation() {
    let kinds = history(6, &[Kind::BreakUp]);
    let state = evaluate(&snapshot(&sequence(&kinds, &[])), &neutral(), &[]).next_state;
    let mut next = kinds.clone();
    next.push(Kind::Wide);
    let evaluation = evaluate(&snapshot(&sequence(&next, &[])), &state, &[]);
    assert_eq!(evaluation.directives[0].kind, DirectiveKind::ExpireEntry);
    assert_eq!(evaluation.directives[0].reason, "SQUEEZE_NEXT_OPEN_MISSED");
}

#[test]
fn a_consumed_release_is_never_reused() {
    let kinds = history(6, &[Kind::BreakUp]);
    let snapshot = snapshot(&sequence(&kinds, &[]));
    let mut state = evaluate(&snapshot, &neutral(), &[]).next_state;
    let signal = state.active_signal.clone().unwrap();
    let sequence = state.next_event_sequence;
    apply_terminal_event(
        &mut state,
        &SignalEvent {
            event_id: Uuid::nil(),
            signal_id: signal.signal_id,
            sequence,
            kind: SignalEventKind::Expired,
            effective_at: signal.effective_at,
            recorded_at: signal.effective_at,
            virtual_trade_id: None,
            price: None,
            reason: None,
        },
    )
    .unwrap();
    assert!(state.active_signal.is_none());
    assert!(evaluate(&snapshot, &state, &[]).signal.is_none());
}
