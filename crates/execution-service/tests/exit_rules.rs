use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanFeedback, PlanKind,
    PlanProfileKey, PriceZone, SignalDefinition, SignalExecutionPlan, TargetLeg,
};
use execution_domain::{
    Ambiguity, ExecutionModel, PositionStage, ReconciliationSource, TradeState,
    VirtualTradeEventKind,
};
use execution_service::{TrackerInput, TrackerRuntime};
use market_domain::{AggregateTrade, Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

#[test]
fn target_and_initial_stop_close_trade_at_configured_level_by_event_order() {
    let mut target_runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let target = target_runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_131)));
    let target_trade = target.virtual_trade_write.unwrap().trade;
    assert_eq!(target_trade.trade_state, TradeState::Closed);
    assert_eq!(target_trade.exit_price, Some(Decimal::new(4_130, 0)));
    assert!(matches!(
        target.trade_events[0].kind,
        VirtualTradeEventKind::TargetTouched
    ));

    let mut stop_runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let stop = stop_runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_089)));
    let stop_trade = stop.virtual_trade_write.unwrap().trade;
    assert_eq!(stop_trade.trade_state, TradeState::Closed);
    assert_eq!(stop_trade.exit_price, Some(Decimal::new(4_090, 0)));
    assert!(matches!(
        stop.trade_events[0].kind,
        VirtualTradeEventKind::StopTouched
    ));
}

#[test]
fn sell_trade_closes_when_price_reaches_initial_stop() {
    let mut runtime = opened_runtime(PlanKind::Quasimodo, Direction::Sell);

    let stop = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_131)));
    let stop_trade = stop.virtual_trade_write.unwrap().trade;

    assert_eq!(stop_trade.trade_state, TradeState::Closed);
    assert_eq!(stop_trade.exit_price, Some(Decimal::new(4_130, 0)));
    assert!(matches!(
        stop.trade_events[0].kind,
        VirtualTradeEventKind::StopTouched
    ));
}

#[test]
fn missed_exact_ticks_close_from_closed_candle_range_conservatively() {
    let mut runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let terminal_candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        Decimal::new(4_105, 0),
        Decimal::new(4_131, 0),
        Decimal::new(4_089, 0),
        Decimal::new(4_120, 0),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(terminal_candle));

    let trade = transition.virtual_trade_write.unwrap().trade;
    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.exit_price, Some(Decimal::new(4_090, 0)));
    assert_eq!(trade.execution_model, ExecutionModel::OhlcConservative);
    assert_eq!(
        trade.reconciliation_source,
        Some(ReconciliationSource::OhlcReconciled)
    );
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(
        transition.trade_events[0].kind,
        VirtualTradeEventKind::StopTouched
    );
    assert_eq!(
        transition.monitoring_events[0].event_type,
        "VIRTUAL_TRADE_CLOSED"
    );
}

#[test]
fn ohlc_partial_hard_stop_wins_before_tp1() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        Decimal::new(4_105, 0),
        Decimal::new(4_121, 0),
        Decimal::new(4_089, 0),
        Decimal::new(4_115, 0),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(candle));
    let closed = transition.virtual_trade_write.unwrap().trade;

    assert_eq!(closed.trade_state, TradeState::Closed);
    assert_eq!(closed.exit_price, Some(Decimal::new(4_090, 0)));
    assert_eq!(closed.realized_fraction, Decimal::ONE);
    assert_eq!(closed.remaining_fraction, Decimal::ZERO);
    assert_eq!(closed.realized_r, -Decimal::ONE);
    assert_eq!(closed.gross_r, Some(-Decimal::ONE));
    assert_eq!(closed.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(transition.trade_events.len(), 1);
    assert_eq!(
        transition.trade_events[0].kind,
        VirtualTradeEventKind::StopTouched
    );
}

#[test]
fn ohlc_partial_runs_tp1_then_tp2_when_hard_stop_is_absent() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        Decimal::new(4_110, 0),
        Decimal::new(4_130, 0),
        Decimal::new(4_110, 0),
        Decimal::new(4_125, 0),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(candle));
    let closed = transition.virtual_trade_write.unwrap().trade;

    assert_eq!(closed.trade_state, TradeState::Closed);
    assert_eq!(closed.position_stage, PositionStage::Runner);
    assert_eq!(closed.exit_price, Some(Decimal::new(4_128, 0)));
    assert_eq!(closed.remaining_fraction, Decimal::ZERO);
    assert_eq!(closed.realized_fraction, Decimal::ONE);
    assert_eq!(closed.realized_r, Decimal::new(1_266_667, 6));
    assert_eq!(closed.gross_r, Some(Decimal::new(1_266_667, 6)));
    assert_eq!(
        transition
            .trade_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        [
            VirtualTradeEventKind::PartialTargetTouched,
            VirtualTradeEventKind::RunnerActivated,
            VirtualTradeEventKind::TargetTouched,
        ]
    );
}

#[test]
fn ohlc_partial_tp1_and_new_breakeven_stop_choose_conservative_runner_exit() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        Decimal::new(4_110, 0),
        Decimal::new(4_121, 0),
        Decimal::new(4_100, 0),
        Decimal::new(4_115, 0),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(candle));
    let closed = transition.virtual_trade_write.unwrap().trade;

    assert_eq!(closed.trade_state, TradeState::Closed);
    assert_eq!(closed.position_stage, PositionStage::Runner);
    assert_eq!(closed.exit_price, Some(Decimal::new(4_105, 0)));
    assert_eq!(closed.realized_r, Decimal::new(5, 1));
    assert_eq!(closed.gross_r, Some(Decimal::new(5, 1)));
    assert_eq!(closed.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(
        transition
            .trade_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        [
            VirtualTradeEventKind::PartialTargetTouched,
            VirtualTradeEventKind::RunnerActivated,
            VirtualTradeEventKind::StopTouched,
        ]
    );
}

#[test]
fn ohlc_partial_tp1_and_breakeven_equality_is_conservative_for_buy_and_sell() {
    for direction in [Direction::Buy, Direction::Sell] {
        let mut runtime = planned_opened_runtime(direction);
        let (open, high, low, close, breakeven) = match direction {
            Direction::Buy => (4_110, 4_121, 4_105, 4_115, 4_105),
            Direction::Sell => (4_100, 4_105, 4_089, 4_095, 4_105),
            Direction::None => unreachable!(),
        };
        let candle = Candle::new(
            symbol(),
            Timeframe::M1,
            2_000,
            61_999,
            Decimal::new(open, 0),
            Decimal::new(high, 0),
            Decimal::new(low, 0),
            Decimal::new(close, 0),
        )
        .unwrap();

        let transition = runtime.apply(TrackerInput::OhlcReconciliation(candle));
        let closed = transition.virtual_trade_write.unwrap().trade;

        assert_eq!(closed.trade_state, TradeState::Closed);
        assert_eq!(closed.exit_price, Some(Decimal::new(breakeven, 0)));
        assert_eq!(closed.gross_r, Some(Decimal::new(5, 1)));
        assert_eq!(closed.ambiguity, Ambiguity::Ambiguous);
    }
}

#[test]
fn ohlc_partial_ordered_live_ticks_keep_exact_non_ambiguous_result() {
    let mut runtime = planned_opened_runtime(Direction::Buy);

    let partial = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
    assert_eq!(
        partial.virtual_trade_write.unwrap().trade.position_stage,
        PositionStage::Runner
    );

    let stopped = runtime.apply(TrackerInput::AggregateTrade(trade(12, 4_105)));
    let closed = stopped.virtual_trade_write.unwrap().trade;

    assert_eq!(closed.trade_state, TradeState::Closed);
    assert_eq!(closed.exit_price, Some(Decimal::new(4_105, 0)));
    assert_eq!(closed.gross_r, Some(Decimal::new(5, 1)));
    assert_eq!(closed.ambiguity, Ambiguity::None);
    assert_eq!(stopped.trade_events.len(), 1);
    assert_eq!(
        stopped.trade_events[0].kind,
        VirtualTradeEventKind::StopTouched
    );
}

#[test]
fn ohlc_reconciliation_closes_entry_candle_conservatively_when_exact_ticks_are_missing() {
    let mut runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let overlapping_candle = Candle::new(
        symbol(),
        Timeframe::M1,
        500,
        60_499,
        Decimal::new(4_105, 0),
        Decimal::new(4_131, 0),
        Decimal::new(4_089, 0),
        Decimal::new(4_120, 0),
    )
    .unwrap();

    let transition = runtime.apply(TrackerInput::OhlcReconciliation(overlapping_candle));

    let trade = transition.virtual_trade_write.unwrap().trade;
    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.exit_price, Some(Decimal::new(4_090, 0)));
    assert_eq!(trade.execution_model, ExecutionModel::OhlcConservative);
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn terminal_close_event_payload_keeps_overlay_identity_fields() {
    let mut runtime = opened_runtime(PlanKind::Quasimodo, Direction::Sell);

    let closed = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_131)));
    let payload = &closed.monitoring_events[0].payload;

    assert_eq!(
        closed.monitoring_events[0].event_type,
        "VIRTUAL_TRADE_CLOSED"
    );
    assert_eq!(payload["plan"], "QUASIMODO");
    assert_eq!(payload["timeframe"], "1m");
    assert_eq!(payload["interval"], "1m");
    assert_eq!(payload["direction"], "SELL");
    assert_eq!(payload["tradeState"], "CLOSED");
    assert_eq!(payload["exitReason"], "initial stop touched");
}

#[test]
fn one_terminal_tick_closes_all_open_trades_for_symbol() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Ict,
        Direction::Buy,
    )));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, 4_105)));
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Smc,
        Direction::Buy,
    )));
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_105)));

    let closed = runtime.apply(TrackerInput::AggregateTrade(trade(12, 4_131)));

    assert_eq!(
        closed.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
    assert_eq!(closed.virtual_trade_writes.len(), 1);
    assert_eq!(
        closed.virtual_trade_writes[0].trade.trade_state,
        TradeState::Closed
    );
    assert_eq!(closed.trade_events.len(), 2);
    assert_eq!(closed.monitoring_events.len(), 2);
    assert!(
        runtime
            .active_trade(&key(PlanKind::Ict))
            .unwrap()
            .trade_state
            .is_terminal()
    );
    assert!(
        runtime
            .active_trade(&key(PlanKind::Smc))
            .unwrap()
            .trade_state
            .is_terminal()
    );
}

#[test]
fn one_terminal_tick_invalidates_all_unentered_recommendations_for_symbol() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Ict,
        Direction::Buy,
    )));
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Smc,
        Direction::Buy,
    )));

    let invalidated = runtime.apply(TrackerInput::AggregateTrade(trade(12, 4_131)));

    assert_eq!(invalidated.signal_events.len(), 2);
    assert_eq!(invalidated.monitoring_events.len(), 2);
    assert!(runtime.active_recommendation(&key(PlanKind::Ict)).is_none());
    assert!(runtime.active_recommendation(&key(PlanKind::Smc)).is_none());
}

#[test]
fn reference_price_invalidates_stale_recommendations_without_market_cursor() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Ict,
        Direction::Buy,
    )));
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::Smc,
        Direction::Sell,
    )));

    let invalidated = runtime.apply(TrackerInput::ReferencePrice {
        symbol: symbol(),
        time: 2_000,
        price: Decimal::new(4_131, 0),
    });

    assert_eq!(invalidated.signal_events.len(), 2);
    assert_eq!(invalidated.monitoring_events.len(), 2);
    assert!(invalidated.market_cursor.is_none());
    assert!(runtime.active_recommendation(&key(PlanKind::Ict)).is_none());
    assert!(runtime.active_recommendation(&key(PlanKind::Smc)).is_none());
}

#[test]
fn move_stop_creates_trailing_stop_and_reference_level_does_not_execute() {
    let mut runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    let moved = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_105, 0)),
    )));
    assert_eq!(
        moved.virtual_trade_write.unwrap().trade.active_stop,
        Decimal::new(4_105, 0)
    );

    let reference_only = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
    assert!(reference_only.virtual_trade_write.is_none());

    let closed = runtime.apply(TrackerInput::AggregateTrade(trade(12, 4_104)));
    assert_eq!(
        closed.virtual_trade_write.unwrap().trade.exit_price,
        Some(Decimal::new(4_105, 0))
    );
}

#[test]
fn runner_buy_move_stop_is_monotonic_and_never_crosses_breakeven_backward() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_110, 0)),
    )));

    for stale_price in [Decimal::new(4_107, 0), Decimal::new(4_100, 0)] {
        let ignored = runtime.apply(TrackerInput::Directive(directive(
            DirectiveKind::MoveStop,
            trade_id,
            Some(stale_price),
        )));

        assert!(ignored.virtual_trade_write.is_none());
        assert!(ignored.trade_events.is_empty());
        assert_eq!(ignored.consumed_directive_ids.len(), 1);
        let payload = &ignored.monitoring_events[0].payload;
        assert_eq!(payload["positionStage"], "RUNNER");
        assert_eq!(payload["remainingFraction"], "0.5");
        assert_eq!(payload["hardStop"], "4090");
        assert_eq!(payload["activeStop"], "4110");
        assert_eq!(payload["tp1Filled"], true);
        assert_eq!(payload["tp2Zone"]["low"], "4128");
        assert!(payload["directiveId"].is_string());
        assert_eq!(payload["directiveKind"], "MOVE_STOP");
        assert_eq!(payload["requestedPrice"], stale_price.to_string());
        assert_eq!(
            runtime
                .active_trade(&key(PlanKind::Ict))
                .unwrap()
                .active_stop,
            Decimal::new(4_110, 0)
        );
    }
}

#[test]
fn runner_move_stop_rejects_out_of_order_and_wrong_version_directives() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_110, 0)),
    )));

    let mut out_of_order = directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_115, 0)),
    );
    out_of_order.directive_id = Uuid::now_v7();
    out_of_order.effective_at -= 1;
    out_of_order.source_closed_candle_time -= 1;
    let ignored = runtime.apply(TrackerInput::Directive(out_of_order));
    assert!(ignored.virtual_trade_write.is_none());
    assert_eq!(ignored.consumed_directive_ids.len(), 1);

    let mut wrong_version = directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_116, 0)),
    );
    wrong_version.directive_id = Uuid::now_v7();
    wrong_version.effective_at += 1;
    wrong_version.source_closed_candle_time += 1;
    wrong_version.parameter_version = "param-v0".to_owned();
    let ignored = runtime.apply(TrackerInput::Directive(wrong_version));
    assert!(ignored.virtual_trade_write.is_none());
    assert_eq!(ignored.consumed_directive_ids.len(), 1);

    assert_eq!(
        runtime
            .active_trade(&key(PlanKind::Ict))
            .unwrap()
            .active_stop,
        Decimal::new(4_110, 0)
    );
}

#[test]
fn runner_sell_move_stop_is_monotonic_and_never_crosses_breakeven_backward() {
    let mut runtime = planned_opened_runtime(Direction::Sell);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_090)));
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveStop,
        trade_id,
        Some(Decimal::new(4_100, 0)),
    )));

    for stale_price in [Decimal::new(4_103, 0), Decimal::new(4_110, 0)] {
        let ignored = runtime.apply(TrackerInput::Directive(directive(
            DirectiveKind::MoveStop,
            trade_id,
            Some(stale_price),
        )));

        assert!(ignored.virtual_trade_write.is_none());
        assert!(ignored.trade_events.is_empty());
        assert_eq!(ignored.consumed_directive_ids.len(), 1);
        assert_eq!(
            runtime
                .active_trade(&key(PlanKind::Ict))
                .unwrap()
                .active_stop,
            Decimal::new(4_100, 0)
        );
    }
}

#[test]
fn buy_move_target_preserves_analytical_target_and_moves_tp2_zone_from_low_edge() {
    let mut runtime = planned_opened_runtime(Direction::Buy);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    let moved = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveTarget,
        trade_id,
        Some(Decimal::new(4_150, 0)),
    )));

    let trade = &moved.virtual_trade_write.as_ref().unwrap().trade;
    assert_eq!(trade.trade_state, TradeState::Open);
    assert_eq!(trade.position_stage, PositionStage::Runner);
    assert_eq!(trade.primary_target, Decimal::new(4_130, 0));
    assert_eq!(trade.tp2_zone_low, Some(Decimal::new(4_150, 0)));
    assert_eq!(trade.tp2_zone_high, Some(Decimal::new(4_152, 0)));
    assert_eq!(moved.trade_events[0].payload["primaryTarget"], "4130");
    assert_eq!(moved.trade_events[0].payload["tp2ExecutionEdge"], "4150");
    assert_eq!(moved.trade_events[0].payload["tp2ZoneLow"], "4150");
    assert_eq!(moved.trade_events[0].payload["tp2ZoneHigh"], "4152");
    assert_eq!(
        moved.monitoring_events[0].event_type,
        "VIRTUAL_TRADE_TARGET_MOVED"
    );
    assert_eq!(moved.monitoring_events[0].payload["primaryTarget"], "4130");
    assert_eq!(
        moved.monitoring_events[0].payload["tp2ExecutionEdge"],
        "4150"
    );
    assert_eq!(moved.monitoring_events[0].payload["tp2ZoneLow"], "4150");
    assert_eq!(moved.monitoring_events[0].payload["tp2ZoneHigh"], "4152");
}

#[test]
fn sell_move_target_preserves_analytical_target_and_moves_tp2_zone_from_high_edge() {
    let mut runtime = planned_opened_runtime(Direction::Sell);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_090)));
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;

    let moved = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveTarget,
        trade_id,
        Some(Decimal::new(4_050, 0)),
    )));

    let trade = &moved.virtual_trade_write.as_ref().unwrap().trade;
    assert_eq!(trade.trade_state, TradeState::Open);
    assert_eq!(trade.position_stage, PositionStage::Runner);
    assert_eq!(trade.primary_target, Decimal::new(4_080, 0));
    assert_eq!(trade.tp2_zone_low, Some(Decimal::new(4_048, 0)));
    assert_eq!(trade.tp2_zone_high, Some(Decimal::new(4_050, 0)));
    assert_eq!(moved.trade_events[0].payload["primaryTarget"], "4080");
    assert_eq!(moved.trade_events[0].payload["tp2ExecutionEdge"], "4050");
    assert_eq!(moved.trade_events[0].payload["tp2ZoneLow"], "4048");
    assert_eq!(moved.trade_events[0].payload["tp2ZoneHigh"], "4050");
    assert_eq!(moved.monitoring_events[0].payload["primaryTarget"], "4080");
    assert_eq!(
        moved.monitoring_events[0].payload["tp2ExecutionEdge"],
        "4050"
    );
    assert_eq!(moved.monitoring_events[0].payload["tp2ZoneLow"], "4048");
    assert_eq!(moved.monitoring_events[0].payload["tp2ZoneHigh"], "4050");
}

#[test]
fn legacy_move_target_without_tp2_zone_keeps_single_target_fallback() {
    let mut runtime = opened_runtime(PlanKind::Rsi, Direction::Buy);
    let trade_id = runtime
        .active_trade(&key(PlanKind::Rsi))
        .unwrap()
        .virtual_trade_id;

    let moved = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveTarget,
        trade_id,
        Some(Decimal::new(4_150, 0)),
    )));

    let trade = &moved.virtual_trade_write.as_ref().unwrap().trade;
    assert_eq!(trade.primary_target, Decimal::new(4_150, 0));
    assert_eq!(trade.tp2_zone_low, None);
    assert_eq!(trade.tp2_zone_high, None);
    assert_eq!(moved.trade_events[0].payload["primaryTarget"], "4150");
    assert_eq!(moved.trade_events[0].payload["tp2ExecutionEdge"], "4150");
    assert!(moved.trade_events[0].payload["tp2ZoneLow"].is_null());
    assert!(moved.trade_events[0].payload["tp2ZoneHigh"].is_null());
    assert_eq!(moved.monitoring_events[0].payload["primaryTarget"], "4150");
    assert!(moved.monitoring_events[0].payload["tp2ExecutionEdge"].is_null());
}

#[test]
fn suggest_close_and_emergency_exit_close_active_virtual_trade() {
    for (kind, reason) in [
        (DirectiveKind::SuggestClose, "test directive"),
        (DirectiveKind::EmergencyExit, "test directive"),
    ] {
        let mut runtime = opened_runtime(PlanKind::Rsi, Direction::Buy);
        let trade_id = runtime
            .active_trade(&key(PlanKind::Rsi))
            .unwrap()
            .virtual_trade_id;

        let closed = runtime.apply(TrackerInput::Directive(directive(
            kind,
            trade_id,
            Some(Decimal::new(4_118, 0)),
        )));
        let trade = closed.virtual_trade_write.unwrap().trade;

        assert_eq!(trade.trade_state, TradeState::Closed);
        assert_eq!(trade.exit_price, Some(Decimal::new(4_118, 0)));
        assert_eq!(trade.exit_reason.as_deref(), Some(reason));
        assert_eq!(trade.gross_r, Some(Decimal::new(866_667, 6)));
        assert_eq!(trade.mfe_r, Decimal::new(866_667, 6));
        assert_eq!(trade.mae_r, Decimal::ZERO);
        assert_eq!(
            closed.monitoring_events[0].payload["managementAction"],
            kind.as_str()
        );
        assert!(matches!(
            closed.plan_feedback[0],
            PlanFeedback::TradeClosed { .. }
        ));
        assert_eq!(
            closed.monitoring_events[0].event_type,
            "VIRTUAL_TRADE_CLOSED"
        );
    }
}

#[test]
fn partial_target_terminal_exits_use_weighted_realized_and_gross_r() {
    let cases = [
        (
            "TP2",
            None,
            Decimal::new(4_130, 0),
            Decimal::new(1_266_667, 6),
        ),
        ("STOP", None, Decimal::new(4_105, 0), Decimal::new(5, 1)),
        (
            "SUGGEST_CLOSE",
            Some(DirectiveKind::SuggestClose),
            Decimal::new(4_115, 0),
            Decimal::new(833_333, 6),
        ),
        (
            "EMERGENCY_EXIT",
            Some(DirectiveKind::EmergencyExit),
            Decimal::new(4_095, 0),
            Decimal::new(166_667, 6),
        ),
    ];

    for (name, directive_kind, price, expected_r) in cases {
        let mut runtime = planned_opened_runtime(Direction::Buy);
        let partial = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_120)));
        assert_eq!(
            partial.virtual_trade_write.unwrap().trade.position_stage,
            PositionStage::Runner,
            "{name}"
        );
        let trade_id = runtime
            .active_trade(&key(PlanKind::Ict))
            .unwrap()
            .virtual_trade_id;

        let terminal = if let Some(kind) = directive_kind {
            runtime.apply(TrackerInput::Directive(directive(
                kind,
                trade_id,
                Some(price),
            )))
        } else {
            runtime.apply(TrackerInput::AggregateTrade(
                AggregateTrade::new(symbol(), 12, 12, 12, 1_200, 1_200, price).unwrap(),
            ))
        };
        let closed = terminal.virtual_trade_write.unwrap().trade;

        assert_eq!(closed.trade_state, TradeState::Closed, "{name}");
        assert_eq!(closed.remaining_fraction, Decimal::ZERO, "{name}");
        assert_eq!(closed.realized_fraction, Decimal::ONE, "{name}");
        assert_eq!(closed.realized_r, expected_r, "{name}");
        assert_eq!(closed.gross_r, Some(expected_r), "{name}");
    }
}

#[test]
fn partial_target_sell_tp2_uses_weighted_realized_and_gross_r() {
    let mut runtime = planned_opened_runtime(Direction::Sell);
    runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_090)));

    let terminal = runtime.apply(TrackerInput::AggregateTrade(trade(12, 4_080)));
    let closed = terminal.virtual_trade_write.unwrap().trade;

    assert_eq!(closed.exit_price, Some(Decimal::new(4_082, 0)));
    assert_eq!(closed.remaining_fraction, Decimal::ZERO);
    assert_eq!(closed.realized_fraction, Decimal::ONE);
    assert_eq!(closed.realized_r, Decimal::new(1_266_667, 6));
    assert_eq!(closed.gross_r, Some(Decimal::new(1_266_667, 6)));
}

#[test]
fn exit_at_next_open_and_duplicate_directive_are_idempotent() {
    let mut runtime = opened_runtime(PlanKind::Ict, Direction::Buy);
    let trade_id = runtime
        .active_trade(&key(PlanKind::Ict))
        .unwrap()
        .virtual_trade_id;
    let directive = directive(
        DirectiveKind::ExitAtNextOpen,
        trade_id,
        Some(Decimal::new(4_125, 0)),
    );

    let closed = runtime.apply(TrackerInput::Directive(directive.clone()));
    assert_eq!(
        closed.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
    assert!(matches!(
        closed.plan_feedback[0],
        PlanFeedback::TradeClosed { .. }
    ));

    let duplicate = runtime.apply(TrackerInput::Directive(directive));
    assert!(duplicate.virtual_trade_write.is_none());
    assert!(duplicate.plan_feedback.is_empty());
}

#[test]
fn regime_invalidation_closes_active_trade() {
    let mut runtime = opened_runtime(PlanKind::Smc, Direction::Buy);
    let trade_id = runtime
        .active_trade(&key(PlanKind::Smc))
        .unwrap()
        .virtual_trade_id;

    let closed = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::CancelEntry,
        trade_id,
        Some(Decimal::new(4_100, 0)),
    )));

    assert_eq!(
        closed.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
}

#[test]
fn trend_breakout_reverse_closes_old_trade_and_opens_new_trade_in_one_transition() {
    let mut runtime = opened_runtime(PlanKind::TrendBreakout, Direction::Buy);
    let old_trade_id = runtime
        .active_trade(&key(PlanKind::TrendBreakout))
        .unwrap()
        .virtual_trade_id;
    let reverse_signal = signal(PlanKind::TrendBreakout, Direction::Sell);
    let directive = directive(
        DirectiveKind::ReverseAtNextOpen,
        old_trade_id,
        Some(Decimal::new(4_115, 0)),
    );

    let transition = runtime.apply(TrackerInput::ReverseAtNextOpen {
        directive,
        reverse_signal,
        canonical_open: candle(120_000, 4_115),
    });

    assert_eq!(transition.plan_feedback.len(), 2);
    assert_eq!(
        transition.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
    assert_eq!(transition.virtual_trade_writes.len(), 1);
    assert_eq!(
        transition.virtual_trade_writes[0].trade.trade_state,
        TradeState::Open
    );
}

#[test]
fn reversing_a_replaying_trade_keeps_the_new_trade_replaying() {
    let mut runtime = opened_runtime(PlanKind::TrendBreakout, Direction::Buy);
    let old_trade = runtime
        .active_trade(&key(PlanKind::TrendBreakout))
        .unwrap()
        .clone();
    runtime.begin_replay(old_trade.signal_id, 60_000);

    runtime.apply(TrackerInput::ReverseAtNextOpen {
        directive: directive(
            DirectiveKind::ReverseAtNextOpen,
            old_trade.virtual_trade_id,
            Some(Decimal::new(4_115, 0)),
        ),
        reverse_signal: signal(PlanKind::TrendBreakout, Direction::Sell),
        canonical_open: candle(120_000, 4_115),
    });

    let new_trade = runtime.active_trade(&key(PlanKind::TrendBreakout)).unwrap();
    assert_ne!(new_trade.virtual_trade_id, old_trade.virtual_trade_id);
    assert_eq!(runtime.replay_cursor(), Some(60_000));
}

fn opened_runtime(plan: PlanKind, direction: Direction) -> TrackerRuntime {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(signal(plan, direction)));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, 4_105)));
    runtime
}

fn planned_opened_runtime(direction: Direction) -> TrackerRuntime {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Ict, direction);
    let (hard_stop, thesis, target, tp2_low, tp2_high) = match direction {
        Direction::Buy => (
            Decimal::new(4_090, 0),
            Decimal::new(4_095, 0),
            Decimal::new(4_130, 0),
            Decimal::new(4_128, 0),
            Decimal::new(4_130, 0),
        ),
        Direction::Sell => (
            Decimal::new(4_120, 0),
            Decimal::new(4_115, 0),
            Decimal::new(4_080, 0),
            Decimal::new(4_080, 0),
            Decimal::new(4_082, 0),
        ),
        Direction::None => unreachable!(),
    };
    recommendation.stop_loss = hard_stop;
    recommendation.primary_target = target;
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: thesis,
        hard_stop,
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(tp2_low, tp2_high).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, 4_105)));
    runtime
}

fn signal(plan: PlanKind, direction: Direction) -> SignalDefinition {
    let (stop_loss, primary_target) = match direction {
        Direction::Buy => (Decimal::new(4_090, 0), Decimal::new(4_130, 0)),
        Direction::Sell => (Decimal::new(4_130, 0), Decimal::new(4_090, 0)),
        Direction::None => (Decimal::new(4_090, 0), Decimal::new(4_130, 0)),
    };
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: symbol(),
        plan,
        target_timeframe: Timeframe::M1,
        evaluated_at: 1_000,
        direction,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(4_100, 0), Decimal::new(4_110, 0)).unwrap()),
        projected_entry: Some(Decimal::new(4_105, 0)),
        stop_loss,
        primary_target,
        execution_plan: None,
        reference_levels: vec![analysis_domain::ReferenceLevel {
            kind: "REFERENCE".to_owned(),
            price: Decimal::new(4_120, 0),
        }],
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: None,
        effective_at: 1_000,
        detected_at: 1_000,
        published_at: 1_000,
        release_id: Uuid::now_v7(),
        algorithm_version: "algo-v1".to_owned(),
        parameter_version: "param-v1".to_owned(),
        created_at: 1_000,
        expires_at: None,
    }
}

fn directive(
    kind: DirectiveKind,
    virtual_trade_id: Uuid,
    price: Option<Decimal>,
) -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::from_u128(0x33333333333333333333333333333333),
        signal_id: Uuid::nil(),
        virtual_trade_id: Some(virtual_trade_id),
        kind,
        effective_at: 120_000,
        price,
        reason: "test directive".to_owned(),
        source_closed_candle_time: 119_999,
        algorithm_version: "algo-v1".to_owned(),
        parameter_version: "param-v1".to_owned(),
        reverse_signal: None,
    }
}

fn key(plan: PlanKind) -> PlanProfileKey {
    PlanProfileKey {
        symbol: symbol(),
        plan,
        target_timeframe: Timeframe::M1,
    }
}

fn candle(open_time: i64, open: i64) -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59_999,
        Decimal::new(open, 0),
        Decimal::new(open + 10, 0),
        Decimal::new(open - 10, 0),
        Decimal::new(open, 0),
    )
    .unwrap()
}

fn trade(id: i64, price: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol(),
        id,
        id,
        id,
        id * 100,
        id * 100,
        Decimal::new(price, 0),
    )
    .unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}
