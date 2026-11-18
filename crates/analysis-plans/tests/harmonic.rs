use analysis_core::{ConfirmedSwing, SwingKind};
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HarmonicParameters, HarmonicPattern, MaterializedProfile, PlanKind,
    PlanParameters, PlanProfileKey, PrimitiveVersion, Readiness, SignalDefinition, SignalEventKind,
    TimeframeFacts, VersionIdentity,
};
use analysis_plans::{
    HarmonicConfirmEvaluator, HarmonicEvaluator, PlanEvaluator, PlanRuntimeState,
    PreEntryRejection, validate_harmonic_confirm_entry,
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
    let parameters = if plan == PlanKind::Harmonic {
        PlanParameters::Harmonic(parameters(40))
    } else {
        PlanParameters::HarmonicConfirm(parameters(1))
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

fn snapshot(
    timeframe: Timeframe,
    direction: Direction,
    evaluated: i64,
    close: i64,
    d: Option<i64>,
) -> CanonicalMarketSnapshot {
    let mut swings = vec![
        swing(
            kind(direction, SwingKind::Low),
            1,
            10 * BAR - 1,
            price(direction, 10_000),
        ),
        swing(
            kind(direction, SwingKind::High),
            2,
            20 * BAR - 1,
            price(direction, 11_000),
        ),
        swing(
            kind(direction, SwingKind::Low),
            3,
            30 * BAR - 1,
            price(direction, 10_500),
        ),
        swing(
            kind(direction, SwingKind::High),
            4,
            C_CONFIRMED,
            price(direction, 10_950),
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

#[test]
fn limit_plan_waits_in_the_prz_from_the_c_confirmation_bar() {
    for direction in [Direction::Buy, Direction::Sell] {
        let signal = evaluate_limit(&snapshot(
            Timeframe::M15,
            direction,
            C_CONFIRMED,
            10_800,
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
        // Target 0.382 and extension 0.618 of AD from D_xa 938.2 (AD 161.8).
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
fn limit_plan_is_silent_after_the_c_bar_inside_the_prz_and_off_the_agreed_timeframes() {
    let later = snapshot(
        Timeframe::M15,
        Direction::Buy,
        C_CONFIRMED + BAR,
        10_800,
        None,
    );
    assert!(evaluate_limit(&later).is_none());
    let in_prz = snapshot(Timeframe::M15, Direction::Buy, C_CONFIRMED, 9_390, None);
    assert!(evaluate_limit(&in_prz).is_none());
    let one_minute = snapshot(Timeframe::M1, Direction::Buy, C_CONFIRMED, 10_800, None);
    assert!(evaluate_limit(&one_minute).is_none());
}

#[test]
fn limit_entry_is_cancelled_beyond_c_and_expires_after_forty_bars() {
    let profile = profile(PlanKind::Harmonic, Timeframe::M15);
    let first = HarmonicEvaluator
        .evaluate(
            &profile,
            &snapshot(Timeframe::M15, Direction::Buy, C_CONFIRMED, 10_800, None),
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
                C_CONFIRMED + BAR,
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
            &snapshot(Timeframe::M15, Direction::Buy, expiry, 10_800, None),
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
                C_CONFIRMED + BAR,
                10_800,
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
