use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanKind,
    PreEntryValidation, PriceZone, SignalDefinition,
};
use backtest_engine::{ExecutionAdapter, ExecutionKind, OhlcConservativeExecution};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn candle(open_time: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(open, 0),
        Decimal::new(high, 0),
        Decimal::new(low, 0),
        Decimal::new(close, 0),
    )
    .unwrap()
}

fn signal(
    plan: PlanKind,
    instruction: ExecutionInstruction,
    direction: Direction,
) -> SignalDefinition {
    let release_id = Uuid::now_v7();
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: symbol(),
        plan,
        target_timeframe: Timeframe::M1,
        evaluated_at: 59_999,
        direction,
        execution_instruction: instruction,
        entry_zone: (instruction == ExecutionInstruction::LimitZone)
            .then(|| PriceZone::new(Decimal::new(100, 0), Decimal::new(110, 0)).unwrap()),
        projected_entry: (instruction == ExecutionInstruction::NextOpen)
            .then_some(Decimal::new(100, 0)),
        stop_loss: if direction == Direction::Buy {
            Decimal::new(90, 0)
        } else {
            Decimal::new(110, 0)
        },
        primary_target: if direction == Direction::Buy {
            Decimal::new(120, 0)
        } else {
            Decimal::new(80, 0)
        },
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        effective_at: 60_000,
        detected_at: 60_000,
        published_at: 60_000,
        release_id,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
        created_at: 60_000,
        expires_at: None,
        htf_context: None,
    }
}

fn directive(
    signal_id: Uuid,
    kind: DirectiveKind,
    effective_at: i64,
    price: Option<Decimal>,
) -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::now_v7(),
        signal_id,
        virtual_trade_id: None,
        kind,
        effective_at,
        price,
        reason: "fixture".to_owned(),
        source_closed_candle_time: effective_at - 1,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
        reverse_signal: None,
    }
}

#[test]
fn next_open_and_mean_reversion_validation_are_canonical() {
    let mut model = OhlcConservativeExecution::new();
    let next = candle(60_000, 101, 105, 95, 102);
    let entered = model.on_signal(
        &signal(
            PlanKind::TrendBreakout,
            ExecutionInstruction::NextOpen,
            Direction::Buy,
        ),
        &next,
    );
    assert_eq!(entered[0].price, Decimal::new(101, 0));

    let mut mean = signal(
        PlanKind::MeanReversion,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    mean.pre_entry_validation = Some(PreEntryValidation {
        minimum_rr: Decimal::ONE,
        reject_if_target_crossed: true,
        reject_if_stop_crossed: true,
    });
    let mut model = OhlcConservativeExecution::new();
    let cancelled = model.on_signal(&mean, &candle(60_000, 119, 121, 118, 120));
    assert_eq!(cancelled[0].kind, ExecutionKind::EntryCancelled);
}

#[test]
fn limit_zone_uses_first_reachable_boundary_and_gap_over_zone_does_not_fill() {
    let limit = signal(
        PlanKind::Ict,
        ExecutionInstruction::LimitZone,
        Direction::Buy,
    );
    let mut model = OhlcConservativeExecution::new();
    let fill = model.on_signal(&limit, &candle(60_000, 95, 105, 94, 103));
    assert_eq!(fill[0].price, Decimal::new(100, 0));

    let mut model = OhlcConservativeExecution::new();
    assert!(
        model
            .on_signal(&limit, &candle(60_000, 115, 120, 114, 118))
            .is_empty()
    );
}

#[test]
fn same_candle_target_and_stop_is_stop_first_and_ambiguous() {
    let signal = signal(
        PlanKind::TrendBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    let mut model = OhlcConservativeExecution::new();
    model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
    let exit = model.on_candle(&candle(120_000, 100, 125, 85, 110), &[]);
    assert_eq!(exit[0].kind, ExecutionKind::StopLoss);
    assert!(exit[0].ambiguous_intrabar);
}

#[test]
fn trailing_is_effective_next_candle_and_reference_levels_never_exit() {
    let signal = signal(
        PlanKind::TrendBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    let mut model = OhlcConservativeExecution::new();
    model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
    let trailing = directive(
        signal.signal_id,
        DirectiveKind::MoveStop,
        180_000,
        Some(Decimal::new(105, 0)),
    );
    assert!(
        model
            .on_candle(
                &candle(120_000, 110, 115, 100, 108),
                std::slice::from_ref(&trailing),
            )
            .is_empty()
    );
    let exit = model.on_candle(&candle(180_000, 108, 110, 104, 105), &[trailing]);
    assert_eq!(exit[0].price, Decimal::new(105, 0));
}

#[test]
fn reverse_exits_and_waits_for_a_new_opposite_signal() {
    let signal = signal(
        PlanKind::TrendBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    let mut model = OhlcConservativeExecution::new();
    model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
    let reverse = directive(
        signal.signal_id,
        DirectiveKind::ReverseAtNextOpen,
        120_000,
        None,
    );
    let executions = model.on_candle(&candle(120_000, 105, 106, 104, 105), &[reverse]);
    assert_eq!(
        executions.iter().map(|item| item.kind).collect::<Vec<_>>(),
        vec![ExecutionKind::ReverseExit]
    );

    let exit = directive(
        signal.signal_id,
        DirectiveKind::ExitAtNextOpen,
        180_000,
        None,
    );
    assert!(
        model
            .on_candle(&candle(180_000, 103, 104, 102, 103), &[exit])
            .is_empty()
    );
}

#[test]
fn management_advice_moves_target_or_closes_position_at_next_open() {
    let signal = signal(
        PlanKind::Rsi,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    let mut model = OhlcConservativeExecution::new();
    model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));

    let move_target = directive(
        signal.signal_id,
        DirectiveKind::MoveTarget,
        120_000,
        Some(Decimal::new(120, 0)),
    );
    assert!(
        model
            .on_candle(&candle(120_000, 105, 110, 104, 108), &[move_target])
            .is_empty()
    );
    let target_hit = model.on_candle(&candle(180_000, 108, 121, 107, 119), &[]);
    assert_eq!(target_hit[0].kind, ExecutionKind::TakeProfit);
    assert_eq!(target_hit[0].price, Decimal::new(120, 0));

    let mut model = OhlcConservativeExecution::new();
    model.on_signal(&signal, &candle(240_000, 100, 101, 99, 100));
    let close = directive(signal.signal_id, DirectiveKind::SuggestClose, 300_000, None);
    let executions = model.on_candle(&candle(300_000, 106, 107, 105, 106), &[close]);
    assert_eq!(executions[0].kind, ExecutionKind::SuggestClose);
    assert_eq!(executions[0].price, Decimal::new(106, 0));
}

fn spread_model(reference: backtest_engine::CandlePriceReference) -> OhlcConservativeExecution {
    OhlcConservativeExecution::with_spread(backtest_engine::SpreadConfig {
        price: Decimal::new(6, 1),
        candle_reference: reference,
    })
    .unwrap()
}

#[test]
fn fixed_spread_entries_and_market_exits_use_opposite_quote_sides() {
    use backtest_engine::CandlePriceReference::{Ask, Bid, Mid};
    for (reference, buy_entry, buy_exit, sell_entry, sell_exit) in [
        (Mid, 1003, 997, 997, 1003),
        (Bid, 1006, 1000, 1000, 1006),
        (Ask, 1000, 994, 994, 1000),
    ] {
        for (direction, entry, exit) in [
            (Direction::Buy, buy_entry, buy_exit),
            (Direction::Sell, sell_entry, sell_exit),
        ] {
            let signal = signal(
                PlanKind::TrendBreakout,
                ExecutionInstruction::NextOpen,
                direction,
            );
            let mut model = spread_model(reference);
            let opened = model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
            assert_eq!(opened[0].price, Decimal::new(entry, 1));
            let close = directive(
                signal.signal_id,
                DirectiveKind::ExitAtNextOpen,
                120_000,
                None,
            );
            let closed = model.on_candle(&candle(120_000, 100, 101, 99, 100), &[close]);
            assert_eq!(closed[0].price, Decimal::new(exit, 1));
        }
    }
}

#[test]
fn spread_changes_stop_and_target_triggers_for_long_and_short() {
    use backtest_engine::CandlePriceReference::Mid;
    for direction in [Direction::Buy, Direction::Sell] {
        let signal = signal(
            PlanKind::TrendBreakout,
            ExecutionInstruction::NextOpen,
            direction,
        );
        let mut model = spread_model(Mid);
        model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
        // Reference reaches the target but the executable close quote does not.
        let target_bar = if direction == Direction::Buy {
            candle(120_000, 115, 120, 114, 119)
        } else {
            candle(120_000, 85, 86, 80, 81)
        };
        assert!(model.on_candle(&target_bar, &[]).is_empty());
        let through = if direction == Direction::Buy {
            candle(180_000, 119, 121, 118, 120)
        } else {
            candle(180_000, 81, 82, 79, 80)
        };
        let exit = model.on_candle(&through, &[]);
        assert_eq!(exit[0].kind, ExecutionKind::TakeProfit);
        assert_eq!(exit[0].price, signal.primary_target);

        let mut model = spread_model(Mid);
        model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
        // Reference never crosses the stop, but the closing quote does.
        let mut near_stop = candle(120_000, 100, 101, 99, 100);
        if direction == Direction::Buy {
            near_stop.low = Decimal::new(902, 1);
        } else {
            near_stop.high = Decimal::new(1098, 1);
        }
        assert_eq!(
            model.on_candle(&near_stop, &[])[0].kind,
            ExecutionKind::StopLoss
        );
    }
}

#[test]
fn spread_limit_zone_requires_executable_entry_quote_to_reach_zone() {
    use backtest_engine::CandlePriceReference::Mid;
    for direction in [Direction::Buy, Direction::Sell] {
        let signal = signal(PlanKind::Ict, ExecutionInstruction::LimitZone, direction);
        let mut model = spread_model(Mid);
        let edge = if direction == Direction::Buy {
            candle(60_000, 111, 112, 110, 111)
        } else {
            candle(60_000, 99, 100, 98, 99)
        };
        assert!(model.on_signal(&signal, &edge).is_empty());
        let through = if direction == Direction::Buy {
            candle(120_000, 111, 112, 109, 110)
        } else {
            candle(120_000, 99, 101, 98, 100)
        };
        let fill = model.on_signal(&signal, &through);
        // Short fixture has stop 110 and target 80; lower boundary 100 is valid.
        assert_eq!(fill[0].kind, ExecutionKind::Entry);
        assert_eq!(
            fill[0].price,
            if direction == Direction::Buy {
                Decimal::new(110, 0)
            } else {
                Decimal::new(100, 0)
            }
        );
    }
}

#[test]
fn quote_stop_gap_fills_at_worse_open_and_partial_breakeven_is_not_charged_twice() {
    use backtest_engine::CandlePriceReference::Mid;
    for (direction, open, expected) in [(Direction::Buy, 85, 847), (Direction::Sell, 115, 1153)] {
        let signal = signal(
            PlanKind::TrendBreakout,
            ExecutionInstruction::NextOpen,
            direction,
        );
        let mut model = spread_model(Mid);
        model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100));
        let exits = model.on_candle(&candle(120_000, open, open + 1, open - 1, open), &[]);
        assert_eq!(exits[0].kind, ExecutionKind::StopLoss);
        assert_eq!(exits[0].price, Decimal::new(expected, 1));
    }
    let mut signal = signal(
        PlanKind::TrendBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    signal.execution_plan = Some(analysis_domain::SignalExecutionPlan {
        thesis_invalidation: Decimal::new(95, 0),
        hard_stop: Decimal::new(90, 0),
        tp1: analysis_domain::TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(120, 0), Decimal::new(122, 0)).unwrap(),
    });
    let mut model = spread_model(Mid);
    let entry = model.on_signal(&signal, &candle(60_000, 100, 101, 99, 100))[0].price;
    let partial = model.on_candle(&candle(120_000, 108, 112, 107, 111), &[]);
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].kind, ExecutionKind::PartialTakeProfit);
    assert_eq!(partial[0].price, Decimal::new(1106, 1));
    let runner = model.on_candle(&candle(180_000, 105, 106, 100, 101), &[]);
    assert_eq!(runner[0].price, entry); // Executable bid stop is actual ask entry.
}

#[test]
fn negative_spread_is_rejected() {
    assert!(
        OhlcConservativeExecution::with_spread(backtest_engine::SpreadConfig {
            price: -Decimal::ONE,
            candle_reference: backtest_engine::CandlePriceReference::Mid,
        })
        .is_err()
    );
}

#[test]
fn spread_mean_reversion_validates_actual_entry_and_new_breakeven_has_no_retroactive_gap() {
    use backtest_engine::CandlePriceReference::Mid;
    let mut mean = signal(
        PlanKind::MeanReversion,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    mean.pre_entry_validation = Some(PreEntryValidation {
        minimum_rr: Decimal::TWO,
        reject_if_target_crossed: true,
        reject_if_stop_crossed: true,
    });
    let bar = candle(60_000, 100, 101, 99, 100);
    assert_eq!(
        OhlcConservativeExecution::new().on_signal(&mean, &bar)[0].kind,
        ExecutionKind::Entry
    );
    assert_eq!(
        spread_model(Mid).on_signal(&mean, &bar)[0].kind,
        ExecutionKind::EntryCancelled
    );

    let mut planned = signal(
        PlanKind::TrendBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    planned.execution_plan = Some(analysis_domain::SignalExecutionPlan {
        thesis_invalidation: Decimal::new(95, 0),
        hard_stop: Decimal::new(90, 0),
        tp1: analysis_domain::TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(120, 0), Decimal::new(122, 0)).unwrap(),
    });
    let mut model = spread_model(Mid);
    let entry = model.on_signal(&planned, &bar)[0].price;
    let exits = model.on_candle(&candle(120_000, 100, 112, 99, 110), &[]);
    assert_eq!(exits.len(), 2);
    assert_eq!(exits[0].kind, ExecutionKind::PartialTakeProfit);
    assert_eq!(exits[1].kind, ExecutionKind::StopLoss);
    assert!(exits[1].ambiguous_intrabar);
    assert_eq!(exits[1].price, entry); // Stop was activated after the candle open.
}

#[test]
fn session_breakout_next_open_cancels_through_stop_target_or_minimum_rr_at_zero_spread() {
    // Stop 90, target 120: open 90 is through the stop, 120 through the target,
    // 106 leaves reward 14 for risk 16 (below 1.0), and 100 enters.
    let mut breakout = signal(
        PlanKind::SessionBreakout,
        ExecutionInstruction::NextOpen,
        Direction::Buy,
    );
    breakout.pre_entry_validation = Some(PreEntryValidation {
        minimum_rr: Decimal::ONE,
        reject_if_target_crossed: true,
        reject_if_stop_crossed: true,
    });
    for (open, expected) in [
        (90, ExecutionKind::EntryCancelled),
        (120, ExecutionKind::EntryCancelled),
        (106, ExecutionKind::EntryCancelled),
        (100, ExecutionKind::Entry),
    ] {
        let bar = candle(60_000, open, open + 1, open - 1, open);
        assert_eq!(
            OhlcConservativeExecution::new().on_signal(&breakout, &bar)[0].kind,
            expected,
            "open {open}"
        );
    }
}
