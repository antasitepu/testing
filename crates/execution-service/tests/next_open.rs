use analysis_domain::{
    Direction, ExecutionInstruction, PlanKind, PreEntryValidation, PriceZone, SignalDefinition,
    SignalExecutionPlan, TargetLeg,
};
use execution_domain::{Ambiguity, TradeState, VirtualTradeEventKind};
use execution_service::{
    NextOpenDecision, NextOpenGuard, PendingNextOpen, PreEntryCancellation, TrackerInput,
    TrackerRuntime,
};
use market_domain::{AggregateTrade, Candle, MarketSession, SessionStatus, Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

#[test]
fn rsi_trend_breakout_and_mean_reversion_enter_at_canonical_open() {
    for plan in [
        PlanKind::Rsi,
        PlanKind::TrendBreakout,
        PlanKind::MeanReversion,
    ] {
        let pending = pending(plan, 60_000, Vec::new());

        assert_eq!(
            pending.decide(&candle(60_000, 4_105), Some(&regular_session())),
            NextOpenDecision::Enter {
                time: 60_000,
                price: Decimal::new(4_105, 0)
            }
        );
    }
}

#[test]
fn waits_before_boundary_and_reconciles_when_canonical_open_is_skipped() {
    let pending = pending(PlanKind::Rsi, 60_000, Vec::new());

    assert_eq!(
        pending.decide(&candle(0, 4_100), Some(&regular_session())),
        NextOpenDecision::Wait
    );
    assert_eq!(
        pending.decide(&candle(120_000, 4_110), Some(&regular_session())),
        NextOpenDecision::Reconcile {
            reason: "canonical open skipped expected open time".to_owned()
        }
    );
}

#[test]
fn buffered_first_trade_does_not_replace_binance_kline_open_price() {
    let pending = pending(
        PlanKind::Rsi,
        60_000,
        vec![trade(10, 60_000, 4_200), trade(11, 60_001, 4_201)],
    );

    assert_eq!(
        pending.decide(&candle(60_000, 4_105), Some(&regular_session())),
        NextOpenDecision::Enter {
            time: 60_000,
            price: Decimal::new(4_105, 0)
        }
    );
    assert_eq!(
        pending
            .replay_after_open()
            .iter()
            .map(|event| event.aggregate_trade_id)
            .collect::<Vec<_>>(),
        vec![11]
    );
}

#[test]
fn duplicate_canonical_open_is_ignored_by_guard() {
    let pending = pending(PlanKind::Rsi, 60_000, Vec::new());
    let mut guard = NextOpenGuard::default();
    let open = candle(60_000, 4_105);

    assert!(matches!(
        guard.confirm(&pending, &open, Some(&regular_session())),
        NextOpenDecision::Enter { .. }
    ));
    assert_eq!(
        guard.confirm(&pending, &open, Some(&regular_session())),
        NextOpenDecision::Duplicate
    );
}

#[test]
fn actual_entry_materializes_tp1_from_canonical_open_not_projected_entry() {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Rsi);
    recommendation.projected_entry = Some(Decimal::new(4_105, 0));
    recommendation.stop_loss = Decimal::new(4_090, 0);
    recommendation.primary_target = Decimal::new(4_130, 0);
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: Decimal::new(4_090, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));

    let canonical_open = Candle::new(
        symbol(),
        Timeframe::M1,
        60_000,
        119_999,
        Decimal::new(4_100, 0),
        Decimal::new(4_101, 0),
        Decimal::new(4_099, 0),
        Decimal::new(4_100, 0),
    )
    .unwrap();
    let opened = runtime.apply(TrackerInput::CanonicalOpen(canonical_open));
    let trade = opened.virtual_trade_write.unwrap().trade;

    assert_eq!(trade.actual_entry_price, Decimal::new(4_100, 0));
    assert_eq!(trade.tp1_price, Some(Decimal::new(4_110, 0)));
}

#[test]
fn next_open_entry_candle_tp1_and_breakeven_equality_closes_conservatively() {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Rsi);
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: Decimal::new(4_090, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));

    let transition = runtime.apply(TrackerInput::CanonicalOpen(
        Candle::new(
            symbol(),
            Timeframe::M1,
            60_000,
            119_999,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
            Decimal::new(4_100, 0),
            Decimal::new(4_125, 0),
        )
        .unwrap(),
    ));
    let trade = &transition.virtual_trade_write.as_ref().unwrap().trade;

    assert_eq!(
        transition
            .trade_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            VirtualTradeEventKind::EntryTriggered,
            VirtualTradeEventKind::PartialTargetTouched,
            VirtualTradeEventKind::RunnerActivated,
            VirtualTradeEventKind::StopTouched,
        ]
    );
    assert!(matches!(
        transition.plan_feedback.as_slice(),
        [
            analysis_domain::PlanFeedback::ActualEntry { .. },
            analysis_domain::PlanFeedback::PartialTargetFilled { .. },
            analysis_domain::PlanFeedback::TradeClosed { .. }
        ]
    ));
    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.realized_fraction, Decimal::ONE);
    assert_eq!(trade.remaining_fraction, Decimal::ZERO);
    assert_eq!(trade.exit_price, Some(Decimal::new(4_100, 0)));
    assert_eq!(trade.realized_r, Decimal::new(5, 1));
    assert_eq!(trade.gross_r, Some(Decimal::new(5, 1)));
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn next_open_sell_entry_candle_tp1_and_breakeven_equality_closes_conservatively() {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Rsi);
    recommendation.direction = Direction::Sell;
    recommendation.stop_loss = Decimal::new(4_110, 0);
    recommendation.primary_target = Decimal::new(4_080, 0);
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_105, 0),
        hard_stop: Decimal::new(4_110, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_080, 0), Decimal::new(4_082, 0)).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));

    let transition = runtime.apply(TrackerInput::CanonicalOpen(
        Candle::new(
            symbol(),
            Timeframe::M1,
            60_000,
            119_999,
            Decimal::new(4_100, 0),
            Decimal::new(4_100, 0),
            Decimal::new(4_080, 0),
            Decimal::new(4_085, 0),
        )
        .unwrap(),
    ));
    let trade = transition.virtual_trade_write.unwrap().trade;

    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.exit_price, Some(Decimal::new(4_100, 0)));
    assert_eq!(trade.gross_r, Some(Decimal::new(5, 1)));
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn next_open_runner_tp2_uses_directional_buffered_edge_for_buy_and_sell() {
    for direction in [Direction::Buy, Direction::Sell] {
        let mut runtime = TrackerRuntime::default();
        let mut recommendation = signal(PlanKind::Rsi);
        recommendation.direction = direction;
        let (hard_stop, thesis, analytical_target, zone_low, zone_high, tp1, edge) = match direction
        {
            Direction::Buy => (4_090, 4_095, 4_130, 4_128, 4_130, 4_110, 4_128),
            Direction::Sell => (4_110, 4_105, 4_080, 4_080, 4_082, 4_090, 4_082),
            Direction::None => unreachable!(),
        };
        recommendation.stop_loss = Decimal::new(hard_stop, 0);
        recommendation.primary_target = Decimal::new(analytical_target, 0);
        recommendation.execution_plan = Some(SignalExecutionPlan {
            thesis_invalidation: Decimal::new(thesis, 0),
            hard_stop: Decimal::new(hard_stop, 0),
            tp1: TargetLeg {
                trigger_r: Decimal::ONE,
                fraction: Decimal::new(5, 1),
            },
            tp2_zone: PriceZone::new(Decimal::new(zone_low, 0), Decimal::new(zone_high, 0))
                .unwrap(),
        });
        runtime.apply(TrackerInput::RecommendationOpened(recommendation));
        runtime.apply(TrackerInput::CanonicalOpen(
            Candle::new(
                symbol(),
                Timeframe::M1,
                60_000,
                119_999,
                Decimal::new(4_100, 0),
                Decimal::new(4_101, 0),
                Decimal::new(4_099, 0),
                Decimal::new(4_100, 0),
            )
            .unwrap(),
        ));
        let partial = runtime.apply(TrackerInput::AggregateTrade(trade(10, 120_000, tp1)));
        assert!(
            matches!(
                partial.plan_feedback.as_slice(),
                [analysis_domain::PlanFeedback::PartialTargetFilled { .. }]
            ),
            "direction={direction:?} feedback={:?}",
            partial.plan_feedback
        );

        let terminal = runtime.apply(TrackerInput::OhlcReconciliation(
            match direction {
                Direction::Buy => Candle::new(
                    symbol(),
                    Timeframe::M1,
                    120_000,
                    179_999,
                    Decimal::new(4_120, 0),
                    Decimal::new(edge, 0),
                    Decimal::new(4_110, 0),
                    Decimal::new(4_125, 0),
                ),
                Direction::Sell => Candle::new(
                    symbol(),
                    Timeframe::M1,
                    120_000,
                    179_999,
                    Decimal::new(4_090, 0),
                    Decimal::new(4_095, 0),
                    Decimal::new(edge, 0),
                    Decimal::new(4_085, 0),
                ),
                Direction::None => unreachable!(),
            }
            .unwrap(),
        ));
        let closed = terminal.virtual_trade_write.unwrap().trade;

        assert_eq!(closed.primary_target, Decimal::new(analytical_target, 0));
        assert_eq!(closed.exit_price, Some(Decimal::new(edge, 0)));
        assert_eq!(closed.trade_state, TradeState::Closed);
    }
}

#[test]
fn next_open_entry_candle_tp1_breakeven_tp2_overlap_closes_runner_conservatively() {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Rsi);
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: Decimal::new(4_090, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));

    let transition = runtime.apply(TrackerInput::CanonicalOpen(
        Candle::new(
            symbol(),
            Timeframe::M1,
            60_000,
            119_999,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
            Decimal::new(4_099, 0),
            Decimal::new(4_125, 0),
        )
        .unwrap(),
    ));
    let trade = &transition.virtual_trade_write.as_ref().unwrap().trade;

    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.exit_price, Some(Decimal::new(4_100, 0)));
    assert_eq!(trade.realized_r, Decimal::new(5, 1));
    assert_eq!(trade.gross_r, Some(Decimal::new(5, 1)));
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(
        transition
            .trade_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            VirtualTradeEventKind::EntryTriggered,
            VirtualTradeEventKind::PartialTargetTouched,
            VirtualTradeEventKind::RunnerActivated,
            VirtualTradeEventKind::StopTouched,
        ]
    );
}

#[test]
fn next_open_entry_candle_stop_overlap_remains_hard_stop_first() {
    let mut runtime = TrackerRuntime::default();
    let mut recommendation = signal(PlanKind::Rsi);
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: Decimal::new(4_090, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    runtime.apply(TrackerInput::RecommendationOpened(recommendation));

    let transition = runtime.apply(TrackerInput::CanonicalOpen(
        Candle::new(
            symbol(),
            Timeframe::M1,
            60_000,
            119_999,
            Decimal::new(4_100, 0),
            Decimal::new(4_130, 0),
            Decimal::new(4_089, 0),
            Decimal::new(4_125, 0),
        )
        .unwrap(),
    ));
    let trade = &transition.virtual_trade_write.as_ref().unwrap().trade;

    assert_eq!(
        transition
            .trade_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            VirtualTradeEventKind::EntryTriggered,
            VirtualTradeEventKind::StopTouched,
        ]
    );
    assert!(!transition.plan_feedback.iter().any(|feedback| matches!(
        feedback,
        analysis_domain::PlanFeedback::PartialTargetFilled { .. }
    )));
    assert_eq!(trade.trade_state, TradeState::Closed);
    assert_eq!(trade.realized_r, Decimal::NEGATIVE_ONE);
    assert_eq!(trade.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn no_trading_session_delays_next_open_entry() {
    let pending = pending(PlanKind::Rsi, 60_000, Vec::new());
    let session = MarketSession::new("COMMODITY", SessionStatus::NoTrading, 0, 120_000, 1).unwrap();

    assert_eq!(
        pending.decide(&candle(60_000, 4_105), Some(&session)),
        NextOpenDecision::Wait
    );
}

#[test]
fn delayed_next_open_fills_at_the_first_tradable_bar_after_a_closure() {
    let mut runtime = TrackerRuntime::default();
    let signal = signal(PlanKind::TrendBreakout);
    let signal_id = signal.signal_id;
    runtime.apply(TrackerInput::RecommendationOpened(signal));
    runtime.delay_next_open(signal_id, 180_000);

    let closed_bar = runtime.apply(TrackerInput::CanonicalOpen(candle(60_000, 4_105)));
    assert!(closed_bar.virtual_trade_write.is_none());
    assert!(closed_bar.signal_events.is_empty());

    // Prices past the 4130 target inside the closure do not judge the setup.
    let tick = runtime.apply(TrackerInput::AggregateTrade(trade(1, 61_000, 4_200)));
    assert!(tick.signal_events.is_empty());
    let m1 = runtime.apply(TrackerInput::OhlcReconciliation(candle(120_000, 4_200)));
    assert!(m1.signal_events.is_empty());

    let opened = runtime.apply(TrackerInput::CanonicalOpen(candle(180_000, 4_107)));
    let written = opened.virtual_trade_write.expect("entry at the reopening");
    assert_eq!(written.trade.actual_entry_time, 180_000);
    assert_eq!(written.trade.actual_entry_price, Decimal::new(4_107, 0));
}

#[test]
fn delayed_next_open_expires_when_the_closure_outlasts_the_setup() {
    let mut runtime = TrackerRuntime::default();
    let mut signal = signal(PlanKind::MeanReversion);
    signal.expires_at = Some(120_000);
    let signal_id = signal.signal_id;
    runtime.apply(TrackerInput::RecommendationOpened(signal));
    runtime.delay_next_open(signal_id, 180_000);

    let transition = runtime.apply(TrackerInput::CanonicalOpen(candle(180_000, 4_105)));

    assert!(transition.virtual_trade_write.is_none());
    assert_eq!(
        transition.signal_events[0].kind,
        analysis_domain::SignalEventKind::Expired
    );
}

#[test]
fn mean_reversion_cancels_when_open_crosses_target_stop_or_minimum_rr() {
    let mut target_crossed = pending(PlanKind::MeanReversion, 60_000, Vec::new());
    target_crossed.signal.primary_target = Decimal::new(4_100, 0);
    assert_eq!(
        target_crossed.decide(&candle(60_000, 4_105), Some(&regular_session())),
        NextOpenDecision::Cancel {
            reason: PreEntryCancellation::TargetCrossed
        }
    );

    let mut stop_crossed = pending(PlanKind::MeanReversion, 60_000, Vec::new());
    stop_crossed.signal.stop_loss = Decimal::new(4_110, 0);
    assert_eq!(
        stop_crossed.decide(&candle(60_000, 4_105), Some(&regular_session())),
        NextOpenDecision::Cancel {
            reason: PreEntryCancellation::StopCrossed
        }
    );

    let mut rr_failed = pending(PlanKind::MeanReversion, 60_000, Vec::new());
    rr_failed.signal.primary_target = Decimal::new(4_106, 0);
    assert_eq!(
        rr_failed.decide(&candle(60_000, 4_105), Some(&regular_session())),
        NextOpenDecision::Cancel {
            reason: PreEntryCancellation::InsufficientRiskReward
        }
    );
}

fn pending(
    plan: PlanKind,
    expected_open_time: i64,
    buffered_trades: Vec<AggregateTrade>,
) -> PendingNextOpen {
    PendingNextOpen {
        signal: signal(plan),
        expected_open_time,
        buffered_trades,
    }
}

fn signal(plan: PlanKind) -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: symbol(),
        plan,
        target_timeframe: Timeframe::M1,
        evaluated_at: 59_999,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(Decimal::new(4_105, 0)),
        stop_loss: Decimal::new(4_090, 0),
        primary_target: Decimal::new(4_130, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: Some(PreEntryValidation {
            minimum_rr: Decimal::ONE,
            reject_if_target_crossed: true,
            reject_if_stop_crossed: true,
        }),
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: None,
        effective_at: 60_000,
        detected_at: 59_999,
        published_at: 59_999,
        release_id: Uuid::now_v7(),
        algorithm_version: "algo-v1".to_owned(),
        parameter_version: "param-v1".to_owned(),
        created_at: 59_999,
        expires_at: None,
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

fn trade(id: i64, trade_time: i64, price: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol(),
        id,
        id,
        id,
        trade_time,
        trade_time,
        Decimal::new(price, 0),
    )
    .unwrap()
}

fn regular_session() -> MarketSession {
    MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 120_000, 1).unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

#[test]
fn session_breakout_cancels_like_the_backtest() {
    // Stop 4090, target 4130 (see `signal`).
    let cases = [
        (4_090, Some(PreEntryCancellation::StopCrossed)),
        (4_130, Some(PreEntryCancellation::TargetCrossed)),
        (4_112, Some(PreEntryCancellation::InsufficientRiskReward)),
        (4_105, None),
    ];
    for (open, expected) in cases {
        let pending = pending(PlanKind::SessionBreakout, 60_000, Vec::new());
        let decision = pending.decide(&candle(60_000, open), Some(&regular_session()));
        match expected {
            Some(reason) => {
                assert_eq!(decision, NextOpenDecision::Cancel { reason }, "open {open}")
            }
            None => assert!(
                matches!(decision, NextOpenDecision::Enter { .. }),
                "open {open}"
            ),
        }
    }
}
