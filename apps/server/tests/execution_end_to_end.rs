use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanKind, PlanProfileKey,
    PriceZone, SignalDefinition, SignalEvent, SignalEventKind,
};
use execution_domain::{MarketCursor, TradeState, VirtualTradeEventKind};
use execution_service::{
    RecoveryInput, RecoveryOutcome, RecoveryReason, TrackerInput, TrackerRuntime, choose_recovery,
};
use market_domain::{AggregateTrade, Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

#[test]
fn signal_entry_stop_move_and_terminal_exit_preserve_ordered_transition_contract() {
    let mut runtime = TrackerRuntime::default();
    let signal = signal(PlanKind::Ict, Direction::Buy);
    let opened_signal = runtime.apply(TrackerInput::RecommendationOpened(signal));

    assert_eq!(opened_signal.monitoring_events.len(), 1);
    assert!(opened_signal.virtual_trade_write.is_none());

    let entry = runtime.apply(TrackerInput::AggregateTrade(trade(10, 4_105)));
    let open_trade = entry.virtual_trade_write.as_ref().unwrap().trade.clone();
    assert_eq!(open_trade.trade_state, TradeState::Open);
    assert_eq!(entry.trade_events[0].sequence, 1);
    assert_eq!(
        entry.trade_events[0].kind,
        VirtualTradeEventKind::EntryTriggered
    );
    assert_eq!(
        entry
            .market_cursor
            .as_ref()
            .unwrap()
            .last_processed_agg_trade_id,
        10
    );

    let moved = runtime.apply(TrackerInput::Directive(directive(
        DirectiveKind::MoveStop,
        open_trade.virtual_trade_id,
        Some(dec(4_115)),
        120_000,
    )));
    assert_eq!(moved.consumed_directive_ids.len(), 1);
    assert_eq!(moved.trade_events[0].sequence, 2);
    assert_eq!(moved.trade_events[0].kind, VirtualTradeEventKind::StopMoved);

    let closed = runtime.apply(TrackerInput::AggregateTrade(trade(11, 4_140)));
    assert_eq!(
        closed
            .virtual_trade_write
            .as_ref()
            .unwrap()
            .trade
            .trade_state,
        TradeState::Closed
    );
    assert_eq!(closed.trade_events[0].sequence, 3);
    assert_eq!(
        closed.trade_events[0].kind,
        VirtualTradeEventKind::TargetTouched
    );
    assert_eq!(closed.monitoring_events.len(), 1);
}

#[test]
fn invalidation_before_entry_removes_recommendation_without_virtual_trade() {
    let mut runtime = TrackerRuntime::default();
    let signal = signal(PlanKind::Smc, Direction::Buy);
    let signal_id = signal.signal_id;
    runtime.apply(TrackerInput::RecommendationOpened(signal));

    runtime.apply(TrackerInput::SignalEvent(SignalEvent {
        event_id: Uuid::now_v7(),
        signal_id,
        sequence: 1,
        kind: SignalEventKind::Invalidated,
        effective_at: 2_000,
        recorded_at: 2_000,
        virtual_trade_id: None,
        price: None,
        reason: Some("fixture invalidation".to_owned()),
    }));

    assert!(runtime.active_recommendation(&key(PlanKind::Smc)).is_none());
    assert!(
        runtime
            .apply(TrackerInput::AggregateTrade(trade(10, 4_105)))
            .virtual_trade_write
            .is_none()
    );
}

#[test]
fn trend_breakout_reverse_closes_old_trade_and_opens_new_trade_atomically() {
    let mut runtime = TrackerRuntime::default();
    runtime.apply(TrackerInput::RecommendationOpened(signal(
        PlanKind::TrendBreakout,
        Direction::Buy,
    )));
    runtime.apply(TrackerInput::AggregateTrade(trade(10, 4_105)));
    let old_trade_id = runtime
        .active_trade(&key(PlanKind::TrendBreakout))
        .unwrap()
        .virtual_trade_id;

    let transition = runtime.apply(TrackerInput::ReverseAtNextOpen {
        directive: directive(
            DirectiveKind::ReverseAtNextOpen,
            old_trade_id,
            Some(dec(4_115)),
            180_000,
        ),
        reverse_signal: signal(PlanKind::TrendBreakout, Direction::Sell),
        canonical_open: candle(180_000, 4_115),
    });

    assert_eq!(
        transition.virtual_trade_write.unwrap().trade.trade_state,
        TradeState::Closed
    );
    assert_eq!(transition.virtual_trade_writes.len(), 1);
    assert_eq!(
        transition.virtual_trade_writes[0].trade.trade_state,
        TradeState::Open
    );
    assert_eq!(transition.consumed_directive_ids.len(), 1);
    assert_eq!(transition.trade_events.len(), 2);
}

#[test]
fn restart_recovery_prefers_exact_replay_then_falls_back_to_ambiguous_ohlc_without_duplicates() {
    let exact = choose_recovery(RecoveryInput {
        cursor: Some(cursor(100)),
        exact_trades: vec![trade(100, 4_101), trade(101, 4_102), trade(102, 4_103)],
        ..RecoveryInput::default_for(symbol())
    });
    assert_eq!(
        exact,
        RecoveryOutcome::Exact {
            cursor: cursor(102)
        }
    );

    let duplicate_replay = choose_recovery(RecoveryInput {
        cursor: Some(cursor(102)),
        has_open_trade: true,
        exact_trades: vec![trade(102, 4_103)],
        ..RecoveryInput::default_for(symbol())
    });
    assert_eq!(
        duplicate_replay,
        RecoveryOutcome::OhlcReconciled {
            cursor: Some(cursor(102)),
            ambiguous: false,
            reason: RecoveryReason::NoExactTrades
        }
    );

    let ohlc = choose_recovery(RecoveryInput {
        cursor: Some(cursor(102)),
        exact_unavailable_for_24h: true,
        ohlc_terminal_possible: true,
        ..RecoveryInput::default_for(symbol())
    });
    assert_eq!(
        ohlc,
        RecoveryOutcome::OhlcReconciled {
            cursor: Some(cursor(102)),
            ambiguous: true,
            reason: RecoveryReason::ExactUnavailable
        }
    );
}

fn signal(plan: PlanKind, direction: Direction) -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: symbol(),
        plan,
        target_timeframe: Timeframe::M1,
        evaluated_at: 1_000,
        direction,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(dec(4_100), dec(4_110)).unwrap()),
        projected_entry: Some(dec(4_105)),
        stop_loss: dec(4_090),
        primary_target: dec(4_130),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        effective_at: 1_000,
        detected_at: 1_000,
        published_at: 1_000,
        release_id: Uuid::now_v7(),
        algorithm_version: "algo-v1".to_owned(),
        parameter_version: "param-v1".to_owned(),
        created_at: 1_000,
        expires_at: None,
        htf_context: None,
    }
}

fn directive(
    kind: DirectiveKind,
    virtual_trade_id: Uuid,
    price: Option<Decimal>,
    effective_at: i64,
) -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::now_v7(),
        signal_id: Uuid::nil(),
        virtual_trade_id: Some(virtual_trade_id),
        kind,
        effective_at,
        price,
        reason: "fixture directive".to_owned(),
        source_closed_candle_time: effective_at - 1,
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
        dec(open),
        dec(open + 10),
        dec(open - 10),
        dec(open),
    )
    .unwrap()
}

fn trade(id: i64, price: i64) -> AggregateTrade {
    AggregateTrade::new(symbol(), id, id, id, id * 100, id * 100, dec(price)).unwrap()
}

fn cursor(id: i64) -> MarketCursor {
    MarketCursor {
        symbol: symbol(),
        last_processed_agg_trade_id: id,
        last_trade_time: id * 100,
    }
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn dec(value: i64) -> Decimal {
    Decimal::new(value, 0)
}
