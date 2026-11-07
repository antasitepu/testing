use analysis_domain::{
    Direction, ExecutionInstruction, ManagementDirective, PlanFeedback, PlanKind, PlanProfileKey,
    PriceZone, SignalDefinition, SignalExecutionPlan, TargetLeg,
};
use execution_domain::{
    Ambiguity, PositionStage, ReconciliationSource, TrackingHealth, TradeState,
    VirtualTradeEventKind,
};
use execution_service::{
    ActiveTradeSequence, TrackerInput, TrackerRuntime, TrackerRuntimeError, TrackerSnapshot,
};
use market_domain::{AggregateTrade, Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

#[test]
fn entry_available_recommendation_does_not_create_virtual_trade_until_triggered() {
    let mut runtime = TrackerRuntime::default();
    let signal = limit_signal();

    let transition = runtime.apply(TrackerInput::RecommendationOpened(signal.clone()));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(runtime.active_recommendation(&key()), Some(&signal));
    assert!(runtime.active_trade(&key()).is_none());
    assert!(
        !transition
            .tracker_state
            .unwrap()
            .state
            .to_string()
            .contains("visible")
    );
}

#[test]
fn recommendation_opened_event_contains_overlay_price_levels() {
    let mut runtime = TrackerRuntime::default();

    let transition = runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    let payload = &transition.monitoring_events[0].payload;

    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["direction"], "BUY");
    assert_eq!(payload["execution"], "LIMIT_ZONE");
    assert_eq!(payload["entryLow"], "4100");
    assert_eq!(payload["entryHigh"], "4110");
    assert_eq!(payload["projectedEntry"], "4105");
    assert_eq!(payload["stopLoss"], "4090");
    assert_eq!(payload["primaryTarget"], "4130");
}

#[test]
fn invalid_price_geometry_recommendation_is_rejected_on_open() {
    let mut runtime = TrackerRuntime::default();
    let mut signal = next_open_signal();
    signal.direction = Direction::Sell;
    signal.projected_entry = Some(dec(4087));
    signal.primary_target = dec(4071);
    signal.stop_loss = dec(4079);

    let transition = runtime.apply(TrackerInput::RecommendationOpened(signal));

    assert_eq!(runtime.active_recommendation(&key()), None);
    assert_eq!(
        transition.tracker_state.unwrap().state["setupState"],
        "INVALIDATED"
    );
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Invalidated
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "RECOMMENDATION_INVALIDATED"
    );
    let payload = &transition.monitoring_events[0].payload;
    assert_eq!(payload["plan"], "ICT");
    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["direction"], "SELL");
    assert_eq!(payload["condition"], "INVALIDATED");
    assert_eq!(payload["setupState"], "INVALIDATED");
    assert_eq!(payload["tradeState"], "NOT_ENTERED");
    assert_eq!(payload["reason"], "invalid price geometry");
}

#[test]
fn zone_touch_opens_trade_and_delivers_actual_entry_feedback_once() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let written = transition.virtual_trade_write.unwrap();

    assert_eq!(written.trade.trade_state, TradeState::Open);
    assert_eq!(written.trade.actual_entry_price, dec(4105));
    assert_eq!(
        runtime.active_trade(&key()).unwrap().trade_state,
        TradeState::Open
    );
    assert_eq!(transition.plan_feedback.len(), 1);
    assert!(matches!(
        transition.plan_feedback[0],
        PlanFeedback::ActualEntry { .. }
    ));

    let duplicate = runtime.apply(TrackerInput::AggregateTrade(trade(11, dec(4106))));
    assert!(duplicate.plan_feedback.is_empty());
}

#[test]
fn actual_entry_materializes_tp1_for_limit_zone_and_next_open() {
    let mut limit_runtime = TrackerRuntime::default();
    let mut limit = limit_signal();
    apply_execution_plan(&mut limit, Direction::Buy);
    limit_runtime.apply(TrackerInput::RecommendationOpened(limit));

    let limit_opened = limit_runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4100))));
    let limit_trade = limit_opened.virtual_trade_write.unwrap().trade;
    assert_eq!(limit_trade.actual_entry_price, dec(4100));
    assert_eq!(limit_trade.tp1_price, Some(dec(4110)));

    let mut next_open_runtime = TrackerRuntime::default();
    let mut next_open = limit_signal();
    next_open.execution_instruction = ExecutionInstruction::NextOpen;
    next_open.entry_zone = None;
    next_open.projected_entry = Some(dec(4105));
    apply_execution_plan(&mut next_open, Direction::Buy);
    next_open_runtime.apply(TrackerInput::RecommendationOpened(next_open));

    let next_opened =
        next_open_runtime.apply(TrackerInput::CanonicalOpen(candle(1_000, dec(4100))));
    let next_open_trade = next_opened.virtual_trade_write.unwrap().trade;
    assert_eq!(next_open_trade.actual_entry_price, dec(4100));
    assert_eq!(next_open_trade.tp1_price, Some(dec(4110)));
}

#[test]
fn actual_entry_materializes_tp1_rejects_fill_beyond_buffered_tp2_edge() {
    for instruction in [
        ExecutionInstruction::LimitZone,
        ExecutionInstruction::NextOpen,
    ] {
        let mut runtime = TrackerRuntime::default();
        let mut signal = limit_signal();
        signal.execution_instruction = instruction;
        signal.entry_zone = (instruction == ExecutionInstruction::LimitZone)
            .then(|| PriceZone::new(dec(4100), dec(4101)).unwrap());
        signal.projected_entry = Some(dec(4100));
        signal.primary_target = dec(4112);
        signal.execution_plan = Some(SignalExecutionPlan {
            thesis_invalidation: dec(4095),
            hard_stop: dec(4090),
            tp1: TargetLeg {
                trigger_r: Decimal::ONE,
                fraction: Decimal::new(5, 1),
            },
            tp2_zone: PriceZone::new(dec(4110), dec(4112)).unwrap(),
        });
        runtime.apply(TrackerInput::RecommendationOpened(signal));

        let rejected = match instruction {
            ExecutionInstruction::LimitZone => {
                runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4101))))
            }
            ExecutionInstruction::NextOpen => {
                runtime.apply(TrackerInput::CanonicalOpen(candle(1_000, dec(4101))))
            }
        };

        assert!(rejected.virtual_trade_write.is_none());
        assert_eq!(
            rejected.tracker_state.unwrap().state["reason"],
            "EXECUTION_PLAN_RR_FAILED"
        );
        assert!(runtime.active_trade(&key()).is_none());
    }
}

#[test]
fn partial_target_live_tick_is_atomic_symmetric_and_idempotent() {
    for direction in [Direction::Buy, Direction::Sell] {
        let mut runtime = TrackerRuntime::default();
        let mut signal = limit_signal();
        apply_execution_plan(&mut signal, direction);
        runtime.apply(TrackerInput::RecommendationOpened(signal));
        runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));

        let tp1 = if direction == Direction::Buy {
            dec(4120)
        } else {
            dec(4090)
        };
        let partial = runtime.apply(TrackerInput::AggregateTrade(trade(11, tp1)));
        let runner = partial.virtual_trade_write.as_ref().unwrap().trade.clone();

        assert_eq!(runner.position_stage, PositionStage::Runner);
        assert_eq!(runner.remaining_fraction, Decimal::new(5, 1));
        assert_eq!(runner.realized_fraction, Decimal::new(5, 1));
        assert_eq!(runner.realized_r, Decimal::new(5, 1));
        assert_eq!(runner.tp1_filled_at, Some(1_100));
        match direction {
            Direction::Buy => assert!(runner.active_stop >= runner.actual_entry_price),
            Direction::Sell => assert!(runner.active_stop <= runner.actual_entry_price),
            Direction::None => unreachable!(),
        }
        assert_eq!(
            partial
                .trade_events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            [
                VirtualTradeEventKind::PartialTargetTouched,
                VirtualTradeEventKind::RunnerActivated,
            ]
        );
        assert_eq!(partial.plan_feedback.len(), 1);
        assert!(matches!(
            partial.plan_feedback[0],
            PlanFeedback::PartialTargetFilled {
                realized_fraction,
                remaining_fraction,
                ..
            } if realized_fraction == Decimal::new(5, 1)
                && remaining_fraction == Decimal::new(5, 1)
        ));
        assert_eq!(partial.monitoring_events.len(), 1);

        let duplicate = runtime.apply(TrackerInput::AggregateTrade(trade(12, tp1)));
        assert!(duplicate.virtual_trade_write.is_none());
        assert!(duplicate.trade_events.is_empty());
        assert!(duplicate.plan_feedback.is_empty());
    }
}

#[test]
fn partial_target_restart_does_not_fill_runner_twice() {
    let mut source = TrackerRuntime::default();
    let mut signal = limit_signal();
    apply_execution_plan(&mut signal, Direction::Buy);
    source.apply(TrackerInput::RecommendationOpened(signal));
    source.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let runner = source
        .apply(TrackerInput::AggregateTrade(trade(11, dec(4120))))
        .virtual_trade_write
        .unwrap()
        .trade;

    let mut restored = TrackerRuntime::default();
    restored.restore_snapshot(TrackerSnapshot {
        active_trades: vec![runner.clone()],
        active_trade_sequences: vec![ActiveTradeSequence {
            virtual_trade_id: runner.virtual_trade_id,
            next_sequence: 4,
        }],
        ..TrackerSnapshot::default()
    });

    let duplicate = restored.apply(TrackerInput::AggregateTrade(trade(12, dec(4120))));
    assert!(duplicate.virtual_trade_write.is_none());
    assert!(duplicate.trade_events.is_empty());
    assert!(duplicate.plan_feedback.is_empty());
    assert_eq!(
        restored.active_trade(&key()).unwrap().position_stage,
        PositionStage::Runner
    );
}

#[test]
fn ohlc_partial_restart_matches_uninterrupted_trade_events_and_feedback() {
    let mut uninterrupted = TrackerRuntime::default();
    let mut signal = limit_signal();
    apply_execution_plan(&mut signal, Direction::Buy);
    uninterrupted.apply(TrackerInput::RecommendationOpened(signal));
    let opened = uninterrupted
        .apply(TrackerInput::AggregateTrade(trade(10, dec(4105))))
        .virtual_trade_write
        .unwrap()
        .trade;

    let mut restored = TrackerRuntime::default();
    restored.restore_snapshot(TrackerSnapshot {
        active_trades: vec![opened.clone()],
        active_trade_sequences: vec![ActiveTradeSequence {
            virtual_trade_id: opened.virtual_trade_id,
            next_sequence: 2,
        }],
        ..TrackerSnapshot::default()
    });
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        dec(4110),
        dec(4121),
        dec(4100),
        dec(4115),
    )
    .unwrap();

    let expected = uninterrupted.apply(TrackerInput::OhlcReconciliation(candle.clone()));
    let actual = restored.apply(TrackerInput::OhlcReconciliation(candle));

    assert_eq!(actual.virtual_trade_write, expected.virtual_trade_write);
    assert_eq!(actual.trade_events, expected.trade_events);
    assert_eq!(actual.signal_events, expected.signal_events);
    assert_eq!(actual.plan_feedback, expected.plan_feedback);
    assert_eq!(
        restored.active_trade(&key()),
        uninterrupted.active_trade(&key())
    );
}

#[test]
fn canonical_open_restart_parity_begins_before_entry() {
    let mut signal = next_open_signal();
    apply_execution_plan(&mut signal, Direction::Buy);
    let snapshot = TrackerSnapshot {
        active_recommendations: vec![signal.clone()],
        ..TrackerSnapshot::default()
    };
    let mut uninterrupted = TrackerRuntime::default();
    uninterrupted.restore_snapshot(snapshot.clone());
    let mut restored = TrackerRuntime::default();
    restored.restore_snapshot(snapshot);
    let open = Candle::new(
        symbol(),
        Timeframe::M1,
        signal.effective_at,
        signal.effective_at + 59_999,
        dec(4100),
        dec(4101),
        dec(4099),
        dec(4100),
    )
    .unwrap();

    let expected = uninterrupted.apply(TrackerInput::CanonicalOpen(open.clone()));
    let actual = restored.apply(TrackerInput::CanonicalOpen(open));

    assert_eq!(actual.virtual_trade_write, expected.virtual_trade_write);
    assert_eq!(actual.trade_events, expected.trade_events);
    assert_eq!(actual.signal_events, expected.signal_events);
    assert_eq!(actual.plan_feedback, expected.plan_feedback);
    assert_eq!(
        restored.active_trade(&key()),
        uninterrupted.active_trade(&key())
    );
}

#[test]
fn non_terminal_aggregate_trade_still_advances_processed_cursor() {
    let mut runtime = TrackerRuntime::default();

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4_100))));

    assert_eq!(
        transition.market_cursor,
        Some(execution_domain::MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 10,
            last_trade_time: 1_000,
        })
    );
    assert!(transition.monitoring_events.is_empty());
}

/// `trade(id, ..)` happens at `id * 100`, so ids 20 to 1999 fall inside this closure.
const CLOSURE: (i64, i64) = (2_000, 200_000);

fn limit_setup_with_a_closure(signal: SignalDefinition) -> TrackerRuntime {
    let mut runtime = TrackerRuntime::default();
    runtime.set_market_closures(vec![CLOSURE]);
    runtime.apply(TrackerInput::RecommendationOpened(signal));
    runtime
}

#[test]
fn limit_setup_fills_only_outside_a_market_closure() {
    let mut runtime = limit_setup_with_a_closure(limit_signal());

    for id in [20, 1_999] {
        let transition = runtime.apply(TrackerInput::AggregateTrade(trade(id, dec(4105))));
        assert!(transition.virtual_trade_write.is_none(), "trade {id}");
        assert!(
            runtime.active_recommendation(&key()).is_some(),
            "trade {id}"
        );
    }

    let reopened = runtime.apply(TrackerInput::AggregateTrade(trade(2_000, dec(4105))));

    assert_eq!(
        reopened
            .virtual_trade_write
            .unwrap()
            .trade
            .actual_entry_time,
        200_000
    );
}

#[test]
fn prices_inside_a_market_closure_do_not_cancel_a_waiting_setup() {
    let mut runtime = limit_setup_with_a_closure(limit_signal());

    for (id, price) in [(20, 4085), (21, 4135)] {
        let transition = runtime.apply(TrackerInput::AggregateTrade(trade(id, dec(price))));
        assert!(transition.tracker_state.is_none(), "price {price}");
        assert!(transition.signal_events.is_empty(), "price {price}");
    }
    let wide = Candle::new(
        symbol(),
        Timeframe::M1,
        60_000,
        119_999,
        dec(4105),
        dec(4140),
        dec(4080),
        dec(4105),
    )
    .unwrap();
    let reconciled = runtime.apply(TrackerInput::OhlcReconciliation(wide));
    assert!(reconciled.tracker_state.is_none());
    let referenced = runtime.apply(TrackerInput::ReferencePrice {
        symbol: symbol(),
        time: 150_000,
        price: dec(4085),
    });
    assert!(referenced.tracker_state.is_none());
    assert!(runtime.active_recommendation(&key()).is_some());

    let after = runtime.apply(TrackerInput::AggregateTrade(trade(2_000, dec(4085))));

    assert_eq!(
        after.tracker_state.unwrap().state["setupState"],
        "INVALIDATED"
    );
}

#[test]
fn a_waiting_setup_still_expires_inside_a_market_closure() {
    let mut signal = limit_signal();
    signal.expires_at = Some(3_000);
    let mut runtime = limit_setup_with_a_closure(signal);

    let before = runtime.apply(TrackerInput::AggregateTrade(trade(29, dec(4105))));
    assert!(before.tracker_state.is_none());

    let expired = runtime.apply(TrackerInput::AggregateTrade(trade(30, dec(4105))));

    assert_eq!(
        expired.tracker_state.unwrap().state["setupState"],
        "EXPIRED"
    );
}

#[test]
fn an_open_trade_is_still_tracked_inside_a_market_closure() {
    let mut runtime = limit_setup_with_a_closure(limit_signal());
    let opened = runtime.apply(TrackerInput::AggregateTrade(trade(15, dec(4105))));
    assert!(opened.virtual_trade_write.is_some());

    let stopped = runtime.apply(TrackerInput::AggregateTrade(trade(25, dec(4085))));

    assert_eq!(
        stopped.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
}

#[test]
fn recommendation_is_invalidated_when_price_reaches_exit_before_entry() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4085))));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(runtime.active_recommendation(&key()), None);
    let state = transition.tracker_state.unwrap();
    assert_eq!(state.state["setupState"], "INVALIDATED");
    assert_eq!(state.state["tradeState"], "NOT_ENTERED");
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Invalidated
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "RECOMMENDATION_INVALIDATED"
    );
    let payload = &transition.monitoring_events[0].payload;
    assert_eq!(payload["plan"], "ICT");
    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["direction"], "BUY");
    assert_eq!(payload["condition"], "INVALIDATED");
    assert_eq!(payload["setupState"], "INVALIDATED");
    assert_eq!(payload["tradeState"], "NOT_ENTERED");
    assert_eq!(payload["reason"], "stop touched before entry");
    assert_eq!(payload["price"], "4085");
}

#[test]
fn missed_exact_ticks_invalidate_recommendation_from_closed_candle_range() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    let terminal_candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        dec(4105),
        dec(4131),
        dec(4089),
        dec(4110),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(terminal_candle));

    assert!(runtime.active_recommendation(&key()).is_none());
    assert_eq!(
        transition.tracker_state.unwrap().state["setupState"],
        "INVALIDATED"
    );
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Invalidated
    );
    assert_eq!(
        transition.signal_events[0].reason.as_deref(),
        Some("stop touched before entry")
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "RECOMMENDATION_INVALIDATED"
    );
}

#[test]
fn recommendation_expires_without_entry_on_first_late_trade() {
    let mut runtime = TrackerRuntime::default();
    let mut signal = limit_signal();
    signal.expires_at = Some(1_500);
    runtime.apply(TrackerInput::RecommendationOpened(signal));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(20, dec(4105))));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(runtime.active_recommendation(&key()), None);
    assert_eq!(
        transition.tracker_state.unwrap().state["setupState"],
        "EXPIRED"
    );
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Expired
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "RECOMMENDATION_EXPIRED"
    );
    let payload = &transition.monitoring_events[0].payload;
    assert_eq!(payload["plan"], "ICT");
    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["direction"], "BUY");
    assert_eq!(payload["condition"], "EXPIRED");
    assert_eq!(payload["setupState"], "EXPIRED");
    assert_eq!(payload["tradeState"], "NOT_ENTERED");
    assert_eq!(payload["reason"], "entry expired");
    assert_eq!(payload["price"], "4105");
}

#[test]
fn next_open_recommendation_enters_on_canonical_open() {
    let mut runtime = TrackerRuntime::default();
    let signal = next_open_signal();
    runtime.apply(TrackerInput::RecommendationOpened(signal));

    let transition = runtime.apply(TrackerInput::CanonicalOpen(candle(1_000, dec(4087))));

    let written = transition.virtual_trade_write.unwrap();
    assert_eq!(written.trade.trade_state, TradeState::Open);
    assert_eq!(written.trade.actual_entry_time, 1_000);
    assert_eq!(written.trade.actual_entry_price, dec(4087));
    assert_eq!(runtime.active_recommendation(&key()), None);
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Triggered
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "VIRTUAL_TRADE_OPENED"
    );
}

#[test]
fn next_open_entry_candle_terminal_range_closes_conservatively() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(next_open_signal()));
    let entry_candle = Candle::new(
        symbol(),
        Timeframe::M1,
        1_000,
        60_999,
        dec(4087),
        dec(4096),
        dec(4078),
        dec(4090),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::CanonicalOpen(entry_candle));

    let trade = transition.virtual_trade_write.unwrap().trade;
    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.exit_price, Some(dec(4079)));
    assert_eq!(
        trade.reconciliation_source,
        Some(ReconciliationSource::OhlcReconciled)
    );
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(transition.trade_events.len(), 2);
    assert_eq!(
        transition.trade_events[0].kind,
        VirtualTradeEventKind::EntryTriggered
    );
    assert_eq!(
        transition.trade_events[1].kind,
        VirtualTradeEventKind::StopTouched
    );
    assert_eq!(transition.monitoring_events.len(), 1);
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "VIRTUAL_TRADE_CLOSED"
    );
}

#[test]
fn terminal_tick_before_entry_time_does_not_close_open_trade() {
    let mut runtime = TrackerRuntime::default();
    let signal = next_open_signal();
    runtime.apply(TrackerInput::RecommendationOpened(signal));
    runtime.apply(TrackerInput::CanonicalOpen(candle(1_000, dec(4087))));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(9, dec(4070))));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(
        runtime.active_trade(&key()).unwrap().trade_state,
        TradeState::Open
    );
}

#[test]
fn close_transition_is_terminal_and_immutable() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    let opened = runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let trade_id = opened.virtual_trade_write.unwrap().trade.virtual_trade_id;

    let close = runtime
        .apply(TrackerInput::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: signal_id(),
            virtual_trade_id: Some(trade_id),
            kind: analysis_domain::DirectiveKind::ExitAtNextOpen,
            effective_at: 2_000,
            price: Some(dec(4120)),
            reason: "target reached".to_owned(),
            source_closed_candle_time: 1_999,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: None,
        }))
        .virtual_trade_write
        .unwrap()
        .trade;

    assert_eq!(close.trade_state, TradeState::Closed);
    assert_eq!(close.gross_r, Some(Decimal::ONE));
    assert_eq!(close.mfe_r, Decimal::ONE);
    assert_eq!(close.mae_r, Decimal::ZERO);
    assert_eq!(
        runtime.active_trade(&key()).unwrap().trade_state,
        TradeState::Closed
    );

    let later = runtime.apply(TrackerInput::AggregateTrade(trade(12, dec(4090))));
    assert!(later.virtual_trade_write.is_none());
    assert_eq!(
        runtime.active_trade(&key()).unwrap().exit_price,
        Some(dec(4120))
    );
}

#[test]
fn restored_active_trade_closes_on_next_terminal_tick() {
    let mut source = TrackerRuntime::default();
    source.apply(TrackerInput::RecommendationOpened(limit_signal()));
    let opened = source
        .apply(TrackerInput::AggregateTrade(trade(10, dec(4105))))
        .virtual_trade_write
        .unwrap()
        .trade;

    let mut restored = TrackerRuntime::default();
    restored.restore_snapshot(TrackerSnapshot {
        active_trades: vec![opened.clone()],
        active_trade_sequences: vec![ActiveTradeSequence {
            virtual_trade_id: opened.virtual_trade_id,
            next_sequence: 2,
        }],
        ..TrackerSnapshot::default()
    });

    let closed = restored.apply(TrackerInput::AggregateTrade(trade(11, dec(4131))));

    let closed_trade = closed.virtual_trade_write.unwrap().trade;
    assert_eq!(closed_trade.trade_state, TradeState::Closed);
    assert_eq!(closed_trade.exit_price, Some(dec(4130)));
    assert_eq!(closed.trade_events[0].sequence, 2);
    assert_eq!(
        closed.monitoring_events[0].payload["virtualTradeId"],
        opened.virtual_trade_id.to_string()
    );
}

#[test]
fn live_prices_accumulate_symmetric_excursions_until_terminal_exit() {
    for direction in [Direction::Buy, Direction::Sell] {
        let mut runtime = TrackerRuntime::default();
        let mut signal = limit_signal();
        signal.direction = direction;
        if direction == Direction::Sell {
            signal.stop_loss = dec(4120);
            signal.primary_target = dec(4080);
        }
        runtime.apply(TrackerInput::RecommendationOpened(signal));
        runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));

        let favorable = if direction == Direction::Buy {
            dec(4120)
        } else {
            dec(4090)
        };
        let adverse = if direction == Direction::Buy {
            dec(4095)
        } else {
            dec(4115)
        };
        assert!(
            runtime
                .apply(TrackerInput::AggregateTrade(trade(11, favorable)))
                .virtual_trade_write
                .is_none()
        );
        runtime.apply(TrackerInput::AggregateTrade(trade(12, adverse)));

        let terminal = if direction == Direction::Buy {
            dec(4090)
        } else {
            dec(4120)
        };
        let closed = runtime
            .apply(TrackerInput::AggregateTrade(trade(13, terminal)))
            .virtual_trade_write
            .unwrap()
            .trade;

        assert_eq!(closed.mfe_r, Decimal::ONE);
        assert_eq!(closed.mae_r, -Decimal::ONE);
    }
}

#[test]
fn non_terminal_ohlc_reconciliation_checkpoints_excursions() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        dec(4105),
        dec(4120),
        dec(4095),
        dec(4110),
    )
    .unwrap();

    let checkpoint = runtime
        .apply(TrackerInput::OhlcReconciliation(candle))
        .virtual_trade_write
        .unwrap()
        .trade;

    assert_eq!(checkpoint.trade_state, TradeState::Open);
    assert_eq!(checkpoint.mfe_r, Decimal::ONE);
    assert_eq!(checkpoint.mae_r, Decimal::new(-666_667, 6));
    assert!(runtime.active_trade(&key()).is_some());
}

#[test]
fn replay_candle_does_not_reach_a_trade_on_live_data() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));

    let transition = runtime.apply(TrackerInput::ReplayCandle(stop_touching_candle()));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(
        runtime.active_trade(&key()).unwrap().trade_state,
        TradeState::Open
    );
}

#[test]
fn replay_candle_already_seen_by_a_trade_is_ignored() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let candle = stop_touching_candle();
    runtime.begin_replay(signal_id(), candle.close_time);

    let transition = runtime.apply(TrackerInput::ReplayCandle(candle.clone()));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(runtime.replay_cursor(), Some(candle.close_time));
}

#[test]
fn closed_trade_no_longer_holds_the_replay_back() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let candle = stop_touching_candle();
    runtime.begin_replay(signal_id(), candle.open_time - 1);

    let transition = runtime.apply(TrackerInput::ReplayCandle(candle));

    assert_eq!(
        transition.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
    assert_eq!(runtime.replay_cursor(), None);
}

#[test]
fn one_active_slot_per_plan_timeframe_replaces_recommendation_but_not_open_trade() {
    let mut runtime = TrackerRuntime::default();
    let first = limit_signal();
    runtime.apply(TrackerInput::RecommendationOpened(first));
    let mut second = limit_signal();
    second.signal_id = Uuid::now_v7();
    second.primary_target = dec(4140);

    let replaced = runtime.apply(TrackerInput::RecommendationOpened(second.clone()));
    assert_eq!(runtime.active_recommendation(&key()), Some(&second));
    assert_eq!(replaced.monitoring_events.len(), 2);
    assert_eq!(
        replaced.monitoring_events[1].event_type,
        "RECOMMENDATION_INVALIDATED"
    );
    let payload = &replaced.monitoring_events[1].payload;
    assert_eq!(payload["plan"], "ICT");
    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["direction"], "BUY");
    assert_eq!(payload["condition"], "INVALIDATED");
    assert_eq!(payload["setupState"], "INVALIDATED");
    assert_eq!(payload["tradeState"], "NOT_ENTERED");
    assert_eq!(payload["reason"], "replaced by newer recommendation");
    assert_eq!(payload["replacementSignalId"], second.signal_id.to_string());

    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    let mut third = limit_signal();
    third.signal_id = Uuid::now_v7();
    let third_id = third.signal_id;
    let transition = runtime.apply(TrackerInput::RecommendationOpened(third));

    assert!(transition.virtual_trade_write.is_none());
    assert!(
        transition.monitoring_events.is_empty(),
        "occupied open-trade slot must not emit RECOMMENDATION_OPENED"
    );
    assert!(
        transition.tracker_state.is_none(),
        "occupied open-trade slot must not persist ENTRY_AVAILABLE tracker state"
    );
    assert_eq!(runtime.active_recommendation(&key()), None);
    assert!(
        runtime
            .active_recommendation(&key())
            .map(|signal| signal.signal_id)
            .is_none()
    );
    assert_ne!(
        runtime.active_trade(&key()).map(|trade| trade.signal_id),
        Some(third_id)
    );
    assert_eq!(
        runtime.active_trade(&key()).unwrap().primary_target,
        dec(4140)
    );
}

#[test]
fn unknown_directive_is_rejected_as_structured_error() {
    let mut runtime = TrackerRuntime::default();

    let error = runtime
        .try_apply(TrackerInput::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: signal_id(),
            virtual_trade_id: Some(Uuid::now_v7()),
            kind: analysis_domain::DirectiveKind::MoveStop,
            effective_at: 2_000,
            price: Some(dec(4095)),
            reason: "unknown".to_owned(),
            source_closed_candle_time: 1_999,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: None,
        }))
        .unwrap_err();

    assert_eq!(error, TrackerRuntimeError::UnknownDirective);
}

#[test]
fn tracker_health_is_owned_by_runtime_not_frontend_visibility() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));

    assert_eq!(runtime.health(symbol()), Some(TrackingHealth::Live));
}

#[test]
fn every_touched_recommendation_enters_on_the_same_trade() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    let second = smc_signal(dec(4100), dec(4110), dec(4090));
    runtime.apply(TrackerInput::RecommendationOpened(second));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));

    let written =
        transition.virtual_trade_write.iter().count() + transition.virtual_trade_writes.len();
    assert_eq!(written, 2);
    for key in [key(), smc_key()] {
        assert_eq!(
            runtime.active_trade(&key).unwrap().trade_state,
            TradeState::Open
        );
        assert_eq!(runtime.active_recommendation(&key), None);
    }
    assert_eq!(
        transition
            .market_cursor
            .unwrap()
            .last_processed_agg_trade_id,
        10
    );
}

#[test]
fn exit_of_one_plan_does_not_suppress_entry_of_another_on_the_same_trade() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(limit_signal()));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, dec(4105))));
    // Second plan's zone sits on the first trade's stop.
    runtime.apply(TrackerInput::RecommendationOpened(smc_signal(
        dec(4085),
        dec(4092),
        dec(4070),
    )));

    let transition = runtime.apply(TrackerInput::AggregateTrade(trade(20, dec(4089))));

    assert!(
        runtime
            .active_trade(&key())
            .unwrap()
            .trade_state
            .is_terminal()
    );
    assert_eq!(
        runtime.active_trade(&smc_key()).unwrap().trade_state,
        TradeState::Open
    );
    let event_types: Vec<_> = transition
        .monitoring_events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect();
    assert!(event_types.contains(&"VIRTUAL_TRADE_CLOSED"));
    assert!(event_types.contains(&"VIRTUAL_TRADE_OPENED"));
}

fn stop_touching_candle() -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        60_000,
        119_999,
        dec(4100),
        dec(4101),
        dec(4080),
        dec(4085),
    )
    .unwrap()
}

fn smc_key() -> PlanProfileKey {
    PlanProfileKey {
        symbol: symbol(),
        plan: PlanKind::Smc,
        target_timeframe: Timeframe::M5,
    }
}

fn smc_signal(zone_low: Decimal, zone_high: Decimal, stop_loss: Decimal) -> SignalDefinition {
    let mut signal = limit_signal();
    signal.signal_id = Uuid::from_u128(0x33333333333333333333333333333333);
    signal.plan = PlanKind::Smc;
    signal.target_timeframe = Timeframe::M5;
    signal.entry_zone = Some(PriceZone::new(zone_low, zone_high).unwrap());
    signal.projected_entry = Some((zone_low + zone_high) / Decimal::TWO);
    signal.stop_loss = stop_loss;
    signal
}

fn key() -> PlanProfileKey {
    PlanProfileKey {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
    }
}

fn signal_id() -> Uuid {
    Uuid::from_u128(0x11111111111111111111111111111111)
}

fn limit_signal() -> SignalDefinition {
    SignalDefinition {
        signal_id: signal_id(),
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at: 1_000,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(dec(4100), dec(4110)).unwrap()),
        projected_entry: Some(dec(4105)),
        stop_loss: dec(4090),
        primary_target: dec(4130),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: dec(2),
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: None,
        effective_at: 1_000,
        detected_at: 1_000,
        published_at: 1_000,
        release_id: Uuid::from_u128(0x22222222222222222222222222222222),
        algorithm_version: "algo-v1".to_owned(),
        parameter_version: "param-v1".to_owned(),
        created_at: 1_000,
        expires_at: None,
    }
}

fn next_open_signal() -> SignalDefinition {
    let mut signal = limit_signal();
    signal.execution_instruction = ExecutionInstruction::NextOpen;
    signal.entry_zone = None;
    signal.projected_entry = Some(dec(4087));
    signal.stop_loss = dec(4079);
    signal.primary_target = dec(4095);
    signal
}

fn apply_execution_plan(signal: &mut SignalDefinition, direction: Direction) {
    signal.direction = direction;
    let (hard_stop, thesis, target, tp2_low, tp2_high) = match direction {
        Direction::Buy => (dec(4090), dec(4095), dec(4130), dec(4128), dec(4130)),
        Direction::Sell => (dec(4120), dec(4115), dec(4080), dec(4080), dec(4082)),
        Direction::None => unreachable!(),
    };
    signal.stop_loss = hard_stop;
    signal.primary_target = target;
    signal.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: thesis,
        hard_stop,
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(tp2_low, tp2_high).unwrap(),
    });
}

fn trade(id: i64, price: Decimal) -> AggregateTrade {
    AggregateTrade::new(symbol(), id, id, id, id * 100, id * 100, price).unwrap()
}

fn candle(open_time: i64, open: Decimal) -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        open,
        open + dec(1),
        open - dec(1),
        open,
    )
    .unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn dec(value: i64) -> Decimal {
    Decimal::new(value, 0)
}
