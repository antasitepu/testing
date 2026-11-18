use analysis_core::{ConfirmedSwing, SwingKind};
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HarmonicParameters, HarmonicPattern, MaterializedProfile, PlanKind,
    PlanParameters, PlanProfileKey, PrimitiveVersion, Readiness, SignalDefinition, SignalEventKind,
    TimeframeFacts, TradeState, VersionIdentity,
};
use analysis_plans::{
    HarmonicConfirmEvaluator, HarmonicEvaluator, HarmonicGbEvaluator, PlanEvaluator,
    PlanRuntimeState, PreEntryRejection, validate_harmonic_confirm_entry,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

const BAR: i64 = 900_000;
const C_CONFIRMED: i64 = 40 * BAR - 1;
const D_CONFIRMED: i64 = 60 * BAR - 1;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn id(kind: FactKind, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M15,
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
    ConfirmedSwing {
        id: id(fact_kind, ordinal, confirmed_at),
        kind,
        label: None,
        price,
        pivot_open_time: confirmed_at + 1 - 3 * BAR,
        confirmed_at,
        source_start_time: confirmed_at + 1 - 5 * BAR,
        source_end_time: confirmed_at,
    }
}

fn parameters(max_bars_valid: i32) -> HarmonicParameters {
    HarmonicParameters {
        patterns: vec![HarmonicPattern::Crab],
        ratio_tolerance: Decimal::new(5, 2),
        min_leg_atr: Decimal::ONE,
        prz_half_width_atr: Decimal::new(25, 2),
        stop_buffer_atr: Decimal::new(25, 2),
        minimum_rr: Decimal::new(15, 1),
        minimum_entry_rr: Decimal::ONE,
        max_bars_valid,
        atr_period: 14,
    }
}

fn profile(plan: PlanKind, timeframe: Timeframe) -> MaterializedProfile {
    let parameters = match plan {
        PlanKind::Harmonic => PlanParameters::Harmonic(parameters(40)),
        PlanKind::HarmonicGb => PlanParameters::HarmonicGb(HarmonicParameters {
            patterns: vec![HarmonicPattern::Gartley, HarmonicPattern::Bat],
            ..parameters(40)
        }),
        _ => PlanParameters::HarmonicConfirm(parameters(1)),
    };
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan,
            target_timeframe: timeframe,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(
            Uuid::from_u128(7),
            "harmonic-fixture-v1",
            "harmonic-fixture-v1",
        )
        .unwrap(),
        parameters,
        required_timeframes: Vec::new(),
        minimum_history_bars: 100,
        warmup_bars: 20,
    }
}

/// Prices of the bullish Crab; the bearish case mirrors them around 2000.
/// X 1000, A 1100, B 1050, C 1095: D_xa 938.2, PRZ 935.7 .. 940.7 with ATR 10.
fn price(direction: Direction, bullish: i64) -> Decimal {
    let value = Decimal::new(bullish, 1);
    if direction == Direction::Buy {
        value
    } else {
        Decimal::new(20_000, 1) - value
    }
}

fn mirrored(direction: Direction, bullish: Decimal) -> Decimal {
    if direction == Direction::Buy {
        bullish
    } else {
        Decimal::new(2_000, 0) - bullish
    }
}

fn kind(direction: Direction, bullish: SwingKind) -> SwingKind {
    match (direction, bullish) {
        (Direction::Buy, kind) => kind,
        (_, SwingKind::High) => SwingKind::Low,
        (_, SwingKind::Low) => SwingKind::High,
    }
}

/// Bullish X, A, B, C in tenths for the Crab fixture.
const CRAB: [i64; 4] = [10_000, 11_000, 10_500, 10_950];

fn snapshot(
    timeframe: Timeframe,
    direction: Direction,
    evaluated: i64,
    close: i64,
    d: Option<i64>,
) -> CanonicalMarketSnapshot {
    snapshot_with(timeframe, direction, evaluated, close, CRAB, d)
}

fn snapshot_with(
    timeframe: Timeframe,
    direction: Direction,
    evaluated: i64,
    close: i64,
    pivots: [i64; 4],
    d: Option<i64>,
) -> CanonicalMarketSnapshot {
    let mut swings = vec![
        swing(
            kind(direction, SwingKind::Low),
            1,
            10 * BAR - 1,
            price(direction, pivots[0]),
        ),
        swing(
            kind(direction, SwingKind::High),
            2,
            20 * BAR - 1,
            price(direction, pivots[1]),
        ),
        swing(
            kind(direction, SwingKind::Low),
            3,
            30 * BAR - 1,
            price(direction, pivots[2]),
        ),
        swing(
            kind(direction, SwingKind::High),
            4,
            C_CONFIRMED,
            price(direction, pivots[3]),
        ),
    ];
    if let Some(d) = d {
        swings.push(swing(
            kind(direction, SwingKind::Low),
            5,
            D_CONFIRMED,
            price(direction, d),
        ));
    }
    let mut facts: Vec<_> = swings
        .into_iter()
        .map(|item| fact(item.id.clone(), item))
        .collect();
    facts.push(fact(
        id(FactKind::Atr, 6, evaluated),
        json!({"value": "10"}),
    ));
    facts.push(fact(
        id(FactKind::MarketClose, 7, evaluated),
        json!({"value": price(direction, close).to_string()}),
    ));
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: timeframe,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: evaluated,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe,
            evaluated_closed_candle_time: evaluated,
            facts,
        },
        higher_timeframes: Vec::new(),
        primitive_versions: vec![PrimitiveVersion {
            timeframe,
            primitive: "fixture".to_owned(),
            algorithm_version: "fixture-v1".to_owned(),
            parameter_version: "fixture-v1".to_owned(),
        }],
    }
}

fn evaluate_limit(snapshot: &CanonicalMarketSnapshot) -> Option<SignalDefinition> {
    let profile = profile(PlanKind::Harmonic, snapshot.target_timeframe);
    HarmonicEvaluator
        .evaluate(
            &profile,
            snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
}

fn evaluate_confirm(snapshot: &CanonicalMarketSnapshot) -> Option<SignalDefinition> {
    let profile = profile(PlanKind::HarmonicConfirm, snapshot.target_timeframe);
    HarmonicConfirmEvaluator
        .evaluate(
            &profile,
            snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
}

fn level(signal: &SignalDefinition, kind: &str) -> Decimal {
    signal
        .reference_levels
        .iter()
        .find(|item| item.kind == kind)
        .unwrap_or_else(|| panic!("missing {kind}"))
        .price
}

/// Bullish close at 990: past X (1000, the primary target), short of the PRZ.
const PAST_X: i64 = 9_900;

#[test]
fn limit_plan_arms_once_the_close_passes_x() {
    for direction in [Direction::Buy, Direction::Sell] {
        let on_c_bar = snapshot(Timeframe::M15, direction, C_CONFIRMED, 10_800, None);
        assert!(evaluate_limit(&on_c_bar).is_none(), "close above X waits");
        let signal = evaluate_limit(&snapshot(
            Timeframe::M15,
            direction,
            C_CONFIRMED + 5 * BAR,
            PAST_X,
            None,
        ))
        .expect("limit signal");
        assert_eq!(signal.plan, PlanKind::Harmonic);
        assert_eq!(signal.direction, direction);
        assert_eq!(
            signal.execution_instruction,
            ExecutionInstruction::LimitZone
        );
        let zone = signal.entry_zone.clone().expect("entry zone");
        let (low, high) = if direction == Direction::Buy {
            (price(direction, 9_357), price(direction, 9_407))
        } else {
            (price(direction, 9_407), price(direction, 9_357))
        };
        assert_eq!((zone.low, zone.high), (low, high));
        assert_eq!(level(&signal, "PRZ_LOW"), low);
        assert_eq!(level(&signal, "PRZ_HIGH"), high);
        assert_eq!(level(&signal, "HARMONIC_C"), price(direction, 10_950));
        // Target 0.382 and extension 0.618 of AD from D_xa 938.2 (AD 161.8):
        // the target is X, already behind the close.
        assert_eq!(
            signal.primary_target,
            mirrored(direction, Decimal::new(10_000_076, 4))
        );
        assert_eq!(
            level(&signal, "TP2_EXTENSION_TARGET"),
            mirrored(direction, Decimal::new(10_381_924, 4))
        );
        match direction {
            Direction::Buy => assert!(signal.stop_loss < low),
            _ => assert!(signal.stop_loss > high),
        }
        assert_eq!(signal.validate_execution_geometry(), Ok(()));
        assert!(signal.risk_reward >= Decimal::new(15, 1));
    }
}

#[test]
fn limit_plan_is_silent_inside_the_prz_after_the_arm_window_and_off_the_agreed_timeframes() {
    let in_prz = snapshot(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + BAR,
        9_390,
        None,
    );
    assert!(evaluate_limit(&in_prz).is_none());
    let last_bar = snapshot(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + 40 * BAR,
        PAST_X,
        None,
    );
    assert!(evaluate_limit(&last_bar).is_some());
    let too_late = snapshot(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + 41 * BAR,
        PAST_X,
        None,
    );
    assert!(evaluate_limit(&too_late).is_none());
    let one_minute = snapshot(
        Timeframe::M1,
        Direction::Buy,
        C_CONFIRMED + BAR,
        PAST_X,
        None,
    );
    assert!(evaluate_limit(&one_minute).is_none());
}

#[test]
fn limit_plan_signals_once_per_c_and_again_for_a_new_c() {
    let profile = profile(PlanKind::Harmonic, Timeframe::M15);
    let first = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + BAR,
                PAST_X,
                None,
            ),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    assert!(first.signal.is_some());
    assert_eq!(
        first.next_state.internal["harmonicSignalledC"],
        json!(C_CONFIRMED)
    );
    // The signal ended (cancelled or expired): a fresh slot keeps only the memory.
    let mut ended = PlanRuntimeState::neutral(&profile);
    ended.internal = first.next_state.internal.clone();
    let again = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + 3 * BAR,
                PAST_X,
                None,
            ),
            &ended,
            &[],
        )
        .unwrap();
    assert!(again.signal.is_none());
    assert_eq!(again.next_state.internal, ended.internal);

    // A higher C (1096) replaces the old one in the zigzag and may signal.
    let new_c_time = C_CONFIRMED + 4 * BAR;
    let mut with_new_c = snapshot(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + 8 * BAR,
        PAST_X,
        None,
    );
    let new_c = swing(SwingKind::High, 8, new_c_time, Decimal::new(10_960, 1));
    with_new_c.target.facts.push(fact(new_c.id.clone(), new_c));
    let renewed = HarmonicEvaluator
        .evaluate(&profile, &with_new_c, &ended, &[])
        .unwrap();
    let signal = renewed.signal.expect("signal for the new C");
    assert_eq!(level(&signal, "HARMONIC_C"), Decimal::new(10_960, 1));
    assert_eq!(
        renewed.next_state.internal["harmonicSignalledC"],
        json!(new_c_time)
    );
}

#[test]
fn remembered_c_survives_a_closed_trade() {
    let profile = profile(PlanKind::Harmonic, Timeframe::M15);
    let first = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + BAR,
                PAST_X,
                None,
            ),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let mut closed = first.next_state.clone();
    closed.trade_state = TradeState::Closed;
    let after = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + 2 * BAR,
                PAST_X,
                None,
            ),
            &closed,
            &[],
        )
        .unwrap();
    assert!(after.next_state.active_signal.is_none());
    assert_eq!(
        after.next_state.internal["harmonicSignalledC"],
        json!(C_CONFIRMED)
    );
}

#[test]
fn limit_entry_is_cancelled_beyond_c_and_expires_after_forty_bars() {
    let profile = profile(PlanKind::Harmonic, Timeframe::M15);
    let first = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + BAR,
                PAST_X,
                None,
            ),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = first.signal.clone().expect("signal");
    let beyond_c = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + 2 * BAR,
                10_960,
                None,
            ),
            &first.next_state,
            &[],
        )
        .unwrap();
    assert_eq!(beyond_c.directives[0].kind, DirectiveKind::CancelEntry);
    assert_eq!(beyond_c.directives[0].reason, "HARMONIC_C_EXCEEDED");
    assert_eq!(beyond_c.events[0].kind, SignalEventKind::Invalidated);

    let expiry = signal.expires_at.expect("expiry");
    assert_eq!(expiry, signal.effective_at + 40 * BAR);
    let expired = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(Timeframe::M15, Direction::Buy, expiry, PAST_X, None),
            &first.next_state,
            &[],
        )
        .unwrap();
    assert_eq!(expired.directives[0].kind, DirectiveKind::ExpireEntry);
    assert_eq!(expired.events[0].kind, SignalEventKind::Expired);

    let waiting = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + 2 * BAR,
                PAST_X,
                None,
            ),
            &first.next_state,
            &[],
        )
        .unwrap();
    assert!(waiting.directives.is_empty());
}

#[test]
fn confirm_plan_enters_at_the_next_open_after_d_is_confirmed_in_the_prz() {
    for direction in [Direction::Buy, Direction::Sell] {
        let signal = evaluate_confirm(&snapshot(
            Timeframe::M15,
            direction,
            D_CONFIRMED,
            9_450,
            Some(9_380),
        ))
        .expect("confirm signal");
        assert_eq!(signal.plan, PlanKind::HarmonicConfirm);
        assert_eq!(signal.direction, direction);
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
        assert_eq!(signal.projected_entry, Some(price(direction, 9_450)));
        assert_eq!(level(&signal, "HARMONIC_D"), price(direction, 9_380));
        let validation = signal.pre_entry_validation.as_ref().expect("validation");
        assert_eq!(validation.minimum_rr, Decimal::ONE);
        assert!(validation.reject_if_stop_crossed && validation.reject_if_target_crossed);
        assert_eq!(signal.expires_at, Some(signal.effective_at + BAR));
        match direction {
            Direction::Buy => assert!(signal.stop_loss < price(direction, 9_380)),
            _ => assert!(signal.stop_loss > price(direction, 9_380)),
        }
        assert_eq!(signal.validate_execution_geometry(), Ok(()));
    }
}

#[test]
fn confirm_plan_is_silent_when_d_is_outside_the_prz_or_not_just_confirmed() {
    let below_prz = snapshot(
        Timeframe::M15,
        Direction::Buy,
        D_CONFIRMED,
        9_450,
        Some(9_300),
    );
    assert!(evaluate_confirm(&below_prz).is_none());
    let above_prz = snapshot(
        Timeframe::M15,
        Direction::Buy,
        D_CONFIRMED,
        9_500,
        Some(9_450),
    );
    assert!(evaluate_confirm(&above_prz).is_none());
    let later = snapshot(
        Timeframe::M15,
        Direction::Buy,
        D_CONFIRMED + BAR,
        9_450,
        Some(9_380),
    );
    assert!(evaluate_confirm(&later).is_none());
    // The limit plan does not fire on the D bar: its last pivot is D, not C.
    let on_d = snapshot(
        Timeframe::M15,
        Direction::Buy,
        D_CONFIRMED,
        9_450,
        Some(9_380),
    );
    assert!(evaluate_limit(&on_d).is_none());
}

#[test]
fn signal_ids_are_deterministic_and_next_open_check_is_plan_specific() {
    let snapshot = snapshot(
        Timeframe::M15,
        Direction::Buy,
        D_CONFIRMED,
        9_450,
        Some(9_380),
    );
    let first = evaluate_confirm(&snapshot).unwrap();
    let second = evaluate_confirm(&snapshot).unwrap();
    assert_eq!(first.signal_id, second.signal_id);
    assert!(validate_harmonic_confirm_entry(&first, first.projected_entry.unwrap()).is_ok());
    let mut other = first.clone();
    other.plan = PlanKind::SessionBreakout;
    assert_eq!(
        validate_harmonic_confirm_entry(&other, Decimal::new(945, 0)),
        Err(PreEntryRejection::WrongPlan)
    );
}

/// Bullish Gartley (D_xa 1021.4, target 1051.4252) and Bat (D_xa 1011.4, target
/// 1045.2452), PRZ +-2.5 with ATR 10.
const GARTLEY: [i64; 4] = [10_000, 11_000, 10_380, 10_800];
const BAT: [i64; 4] = [10_000, 11_000, 10_550, 10_900];

fn evaluate_gb(snapshot: &CanonicalMarketSnapshot) -> Option<SignalDefinition> {
    let profile = profile(PlanKind::HarmonicGb, snapshot.target_timeframe);
    HarmonicGbEvaluator
        .evaluate(
            &profile,
            snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
}

#[test]
fn gartley_bat_plan_arms_once_the_close_passes_the_target() {
    let cases = [
        (GARTLEY, 10_600, 10_400, (10_189, 10_239), 10_514_252),
        (BAT, 10_500, 10_300, (10_089, 10_139), 10_452_452),
    ];
    for (pivots, short_of_target, past_target, (prz_low, prz_high), target) in cases {
        for direction in [Direction::Buy, Direction::Sell] {
            let evaluated = C_CONFIRMED + 2 * BAR;
            let waiting = snapshot_with(
                Timeframe::M15,
                direction,
                evaluated,
                short_of_target,
                pivots,
                None,
            );
            assert!(
                evaluate_gb(&waiting).is_none(),
                "close short of the target waits"
            );
            let signal = evaluate_gb(&snapshot_with(
                Timeframe::M15,
                direction,
                evaluated,
                past_target,
                pivots,
                None,
            ))
            .expect("gartley/bat signal");
            assert_eq!(signal.plan, PlanKind::HarmonicGb);
            assert_eq!(signal.direction, direction);
            assert_eq!(
                signal.execution_instruction,
                ExecutionInstruction::LimitZone
            );
            let zone = signal.entry_zone.clone().expect("entry zone");
            let (low, high) = if direction == Direction::Buy {
                (price(direction, prz_low), price(direction, prz_high))
            } else {
                (price(direction, prz_high), price(direction, prz_low))
            };
            assert_eq!((zone.low, zone.high), (low, high));
            assert_eq!(
                signal.primary_target,
                mirrored(direction, Decimal::new(target, 4))
            );
            assert_eq!(signal.validate_execution_geometry(), Ok(()));
            assert!(signal.risk_reward >= Decimal::new(15, 1));
        }
    }
}

#[test]
fn gartley_bat_plan_is_silent_in_the_prz_late_on_1m_and_on_a_crab_only_structure() {
    let in_prz = snapshot_with(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + BAR,
        10_220,
        GARTLEY,
        None,
    );
    assert!(evaluate_gb(&in_prz).is_none());
    let too_late = snapshot_with(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + 41 * BAR,
        10_400,
        GARTLEY,
        None,
    );
    assert!(evaluate_gb(&too_late).is_none());
    let one_minute = snapshot_with(
        Timeframe::M1,
        Direction::Buy,
        C_CONFIRMED + BAR,
        10_400,
        GARTLEY,
        None,
    );
    assert!(evaluate_gb(&one_minute).is_none());
    // AB/XA 0.55 is a Crab but neither a Gartley nor a Bat.
    let crab_only = [10_000, 11_000, 10_450, 10_890];
    let crab_signal = evaluate_limit(&snapshot_with(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + BAR,
        9_900,
        crab_only,
        None,
    ));
    assert!(crab_signal.is_some());
    for close in [9_900, 10_400] {
        let snapshot = snapshot_with(
            Timeframe::M15,
            Direction::Buy,
            C_CONFIRMED + BAR,
            close,
            crab_only,
            None,
        );
        assert!(evaluate_gb(&snapshot).is_none());
    }
}

#[test]
fn gartley_bat_plan_signals_once_per_c() {
    let profile = profile(PlanKind::HarmonicGb, Timeframe::M15);
    let first = HarmonicGbEvaluator
        .evaluate(
            &profile,
            &snapshot_with(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + BAR,
                10_400,
                GARTLEY,
                None,
            ),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    assert!(first.signal.is_some());
    let mut ended = PlanRuntimeState::neutral(&profile);
    ended.internal = first.next_state.internal.clone();
    let again = HarmonicGbEvaluator
        .evaluate(
            &profile,
            &snapshot_with(
                Timeframe::M15,
                Direction::Buy,
                C_CONFIRMED + 3 * BAR,
                10_400,
                GARTLEY,
                None,
            ),
            &ended,
            &[],
        )
        .unwrap();
    assert!(again.signal.is_none());
}
