use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, HtfAlignment, HtfContext, ManagementDirective,
    MaterializedProfile, PlanFeedback, PlanKind, PlanParameters, PlanProfileKey, PriceZone,
    Readiness, SetupState, SignalDefinition, SignalExecutionPlan, TargetLeg, TradeState,
    VersionIdentity,
};
use analysis_plans::{PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, apply_feedback};
use backtest_engine::{
    BacktestReplay, BacktestRunInput, ChronologicalSplit, FeeConfig, ReplayFrame, SlippageConfig,
};
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

fn frame(open_time: i64, open: i64, high: i64, low: i64, close: i64, warmup: bool) -> ReplayFrame {
    let candle = candle(open_time, open, high, low, close);
    ReplayFrame {
        snapshot: analysis_domain::CanonicalMarketSnapshot::empty(
            symbol(),
            Timeframe::M1,
            Decimal::new(1, 2),
            candle.close_time,
            Readiness::Ready,
        )
        .unwrap(),
        candle,
        directives: Vec::new(),
        warmup,
    }
}

fn run_input(release_id: Uuid) -> BacktestRunInput {
    BacktestRunInput {
        symbol: symbol(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: Timeframe::M1,
        start_time: 0,
        end_time: 359_999,
        release_id,
        algorithm_version: "fixture-algo-v1".to_owned(),
        parameter_version: "fixture-params-v1".to_owned(),
        fees: FeeConfig::default(),
        slippage: SlippageConfig::default(),
        account: None,
        code_commit_hash: "abc1234".to_owned(),
    }
}

fn profile(release_id: Uuid) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::TrendBreakout,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity {
            release_id,
            algorithm_version: "fixture-algo-v1".to_owned(),
            parameter_version: "fixture-params-v1".to_owned(),
        },
        parameters: PlanParameters::TrendBreakout(analysis_domain::TrendBreakoutParameters {
            donchian_period: 2,
            ema_period: 2,
            adx_period: 2,
            minimum_adx: Decimal::ONE,
            higher_timeframe: None,
            atr_period: 2,
            initial_stop_atr_multiplier: Decimal::ONE,
            target_rr: Decimal::TWO,
            trailing_activation_r: Decimal::ONE,
            chandelier_period: 2,
            chandelier_atr_multiplier: Decimal::ONE,
        }),
        required_timeframes: vec![Timeframe::M1],
        minimum_history_bars: 2,
        warmup_bars: 1,
    }
}

fn signal(release_id: Uuid, evaluated_at: i64) -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::from_u128(1000 + evaluated_at as u128),
        symbol: symbol(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: Timeframe::M1,
        evaluated_at,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: Some(PriceZone::new(Decimal::new(100, 0), Decimal::new(101, 0)).unwrap()),
        projected_entry: Some(Decimal::new(100, 0)),
        stop_loss: Decimal::new(90, 0),
        primary_target: Decimal::new(120, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        effective_at: evaluated_at + 1,
        detected_at: evaluated_at + 1,
        published_at: evaluated_at + 1,
        release_id,
        algorithm_version: "fixture-algo-v1".to_owned(),
        parameter_version: "fixture-params-v1".to_owned(),
        created_at: evaluated_at + 1,
        expires_at: None,
        htf_context: Some(HtfContext {
            higher_timeframe: Some(Timeframe::M5),
            direction: Direction::Buy,
            alignment: HtfAlignment::Aligned,
            source: "STRUCTURE".to_owned(),
        }),
    }
}

fn limit_signal(release_id: Uuid, evaluated_at: i64) -> SignalDefinition {
    SignalDefinition {
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(100, 0), Decimal::new(101, 0)).unwrap()),
        projected_entry: None,
        ..signal(release_id, evaluated_at)
    }
}

fn partial_signal(release_id: Uuid, evaluated_at: i64) -> SignalDefinition {
    let mut signal = signal(release_id, evaluated_at);
    signal.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(95, 0),
        hard_stop: Decimal::new(90, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(118, 0), Decimal::new(120, 0)).unwrap(),
    });
    signal
}

struct PartialExitEvaluator {
    release_id: Uuid,
}

impl PlanEvaluator for PartialExitEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }

    fn algorithm_version(&self) -> &'static str {
        "fixture-algo-v1"
    }

    fn evaluate(
        &self,
        _profile: &MaterializedProfile,
        snapshot: &analysis_domain::CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[analysis_domain::PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        let mut next_state = state.clone();
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        let signal = (snapshot.evaluated_closed_candle_time == 59_999)
            .then(|| partial_signal(self.release_id, 59_999));
        if let Some(signal) = &signal {
            next_state.active_signal = Some(signal.clone());
        }
        Ok(PlanEvaluation {
            next_state,
            signal,
            events: Vec::new(),
            directives: Vec::new(),
            rejected_candidates: Vec::new(),
        })
    }
}

struct ScriptedEvaluator {
    release_id: Uuid,
    emit_at: Vec<i64>,
    exit_at: Vec<i64>,
    cancel_at: Vec<i64>,
    limit_entry: bool,
    fail_at: Option<i64>,
}

impl PlanEvaluator for ScriptedEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }

    fn algorithm_version(&self) -> &'static str {
        "fixture-algo-v1"
    }

    fn evaluate(
        &self,
        _profile: &MaterializedProfile,
        snapshot: &analysis_domain::CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[analysis_domain::PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if self.fail_at == Some(snapshot.evaluated_closed_candle_time) {
            return Err(PlanError::Serialization("forced replay failure".to_owned()));
        }
        let mut next_state = state.clone();
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        if next_state.trade_state == TradeState::Closed {
            next_state.active_signal = None;
            next_state.active_virtual_trade_id = None;
            next_state.direction = Direction::None;
            next_state.setup_state = SetupState::Neutral;
        }
        let signal = self
            .emit_at
            .contains(&snapshot.evaluated_closed_candle_time)
            .then(|| {
                if self.limit_entry {
                    limit_signal(self.release_id, snapshot.evaluated_closed_candle_time)
                } else {
                    signal(self.release_id, snapshot.evaluated_closed_candle_time)
                }
            });
        if let Some(signal) = &signal {
            next_state.active_signal = Some(signal.clone());
        }
        let directive_kind = if self
            .exit_at
            .contains(&snapshot.evaluated_closed_candle_time)
        {
            Some(DirectiveKind::ExitAtNextOpen)
        } else if self
            .cancel_at
            .contains(&snapshot.evaluated_closed_candle_time)
        {
            Some(DirectiveKind::CancelEntry)
        } else {
            None
        };
        let directives = if let Some(kind) = directive_kind {
            next_state
                .active_signal
                .as_ref()
                .map(|signal| {
                    vec![ManagementDirective {
                        directive_id: Uuid::from_u128(
                            2000 + snapshot.evaluated_closed_candle_time as u128,
                        ),
                        signal_id: signal.signal_id,
                        virtual_trade_id: None,
                        kind,
                        effective_at: snapshot.evaluated_closed_candle_time + 1,
                        price: None,
                        reason: "SCRIPTED_EXIT".to_owned(),
                        source_closed_candle_time: snapshot.evaluated_closed_candle_time,
                        algorithm_version: signal.algorithm_version.clone(),
                        parameter_version: signal.parameter_version.clone(),
                        reverse_signal: None,
                    }]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(PlanEvaluation {
            next_state,
            signal,
            events: Vec::new(),
            directives,
            rejected_candidates: Vec::new(),
        })
    }
}

#[test]
fn chronological_split_requires_ordered_train_validation_and_test_ranges() {
    assert!(ChronologicalSplit::new(0..=59_999, 60_000..=119_999, 120_000..=179_999).is_ok());
    assert!(ChronologicalSplit::new(60_000..=119_999, 0..=59_999, 120_000..=179_999).is_err());
}

#[test]
fn warmup_frames_do_not_create_trades() {
    let release_id = Uuid::from_u128(1);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 101, 99, 100, true),
                frame(60_000, 100, 121, 89, 110, false),
            ],
        )
        .unwrap();

    assert_eq!(result.trades.len(), 0);
    assert_eq!(result.metrics.total_trades, 0);
}

#[test]
fn signal_on_one_closed_candle_can_only_enter_on_the_next_frame() {
    let release_id = Uuid::from_u128(2);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 125, 85, 110, false),
                frame(60_000, 101, 102, 99, 100, false),
            ],
        )
        .unwrap();

    assert_eq!(result.executions[0].time, 60_000);
    assert_eq!(result.executions[0].price, Decimal::new(101, 0));
    assert!(result.trades.is_empty());
}

#[test]
fn simultaneous_stop_and_target_uses_execution_adapter_barrier_parity() {
    let release_id = Uuid::from_u128(3);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 100, 125, 85, 110, false),
            ],
        )
        .unwrap();

    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.trades[0].exit_reason, "STOP_LOSS");
    assert!(result.trades[0].ambiguous_intrabar);
}

#[test]
fn same_input_is_reproducible() {
    let release_id = Uuid::from_u128(4);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let evaluator = ScriptedEvaluator {
        release_id,
        emit_at: vec![59_999],
        exit_at: Vec::new(),
        cancel_at: Vec::new(),
        limit_entry: false,
        fail_at: None,
    };
    let frames = vec![
        frame(0, 100, 100, 100, 100, false),
        frame(60_000, 100, 101, 99, 100, false),
        frame(120_000, 100, 125, 85, 110, false),
    ];

    let first = replay.run(&evaluator, frames.clone()).unwrap();
    let second = replay.run(&evaluator, frames).unwrap();

    assert_eq!(first, second);
}

#[test]
fn borrowed_replay_matches_owned_without_consuming_frames() {
    let release_id = Uuid::from_u128(40);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let evaluator = ScriptedEvaluator {
        release_id,
        emit_at: vec![59_999],
        exit_at: Vec::new(),
        cancel_at: Vec::new(),
        limit_entry: false,
        fail_at: None,
    };
    let frames = vec![
        frame(0, 100, 100, 100, 100, false),
        frame(60_000, 100, 101, 99, 100, false),
        frame(120_000, 100, 125, 85, 110, false),
    ];

    let borrowed = replay.run_ordered(&evaluator, &frames).unwrap();
    let streamed = replay
        .run_ordered_iter(&evaluator, frames.clone(), frames.len() as u64)
        .unwrap();
    let owned = replay.run(&evaluator, frames.clone()).unwrap();

    assert_eq!(borrowed, owned);
    assert_eq!(streamed, owned);
    assert_eq!(frames.len(), 3);
}

#[test]
fn incremental_replay_session_matches_owned_replay() {
    let release_id = Uuid::from_u128(55);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let evaluator = ScriptedEvaluator {
        release_id,
        emit_at: vec![59_999],
        exit_at: vec![119_999],
        cancel_at: Vec::new(),
        limit_entry: false,
        fail_at: None,
    };
    let frames = vec![
        frame(0, 100, 100, 100, 100, false),
        frame(60_000, 100, 105, 95, 100, false),
        frame(120_000, 106, 107, 100, 105, false),
    ];
    let expected = replay.run(&evaluator, frames.clone()).unwrap();

    let mut session = replay.start_session(&evaluator).unwrap();
    for frame in &frames {
        session.process_frame(frame).unwrap();
    }
    let actual = session.finish(frames.len() as u64);

    assert_eq!(actual, expected);
}

#[test]
fn partial_exit_replay_matches_incremental_session_and_weights_runner_exit() {
    let release_id = Uuid::from_u128(57);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let evaluator = PartialExitEvaluator { release_id };
    let frames = vec![
        frame(0, 100, 100, 100, 100, false),
        frame(60_000, 100, 101, 99, 100, false),
        frame(120_000, 105, 111, 99, 105, false),
    ];
    let expected = replay.run(&evaluator, frames.clone()).unwrap();

    let mut session = replay.start_session(&evaluator).unwrap();
    for frame in &frames[..2] {
        session.process_frame(frame).unwrap();
    }
    session.process_frame(&frames[2]).unwrap();
    let actual = session.finish(frames.len() as u64);

    assert_eq!(actual, expected);
    assert_eq!(actual.trades.len(), 1);
    assert_eq!(actual.trades[0].exit_reason, "STOP_LOSS");
    assert_eq!(actual.trades[0].exit_price, Decimal::new(100, 0));
    assert_eq!(actual.trades[0].gross_r, Decimal::new(5, 1));
    assert!(actual.trades[0].ambiguous_intrabar);
    assert_eq!(
        actual
            .executions
            .iter()
            .map(|execution| execution.kind)
            .collect::<Vec<_>>(),
        [
            backtest_engine::ExecutionKind::Entry,
            backtest_engine::ExecutionKind::PartialTakeProfit,
            backtest_engine::ExecutionKind::StopLoss,
        ]
    );
}

#[test]
fn partial_exit_charges_each_partial_and_terminal_execution_without_changing_legacy_costs() {
    let release_id = Uuid::from_u128(58);
    let mut input = run_input(release_id);
    input.fees.per_execution_r = Decimal::new(1, 1);
    input.slippage.per_execution_r = Decimal::new(5, 2);
    let replay = BacktestReplay::new(input, profile(release_id));
    let partial = replay
        .run(
            &PartialExitEvaluator { release_id },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 105, 111, 99, 105, false),
            ],
        )
        .unwrap();
    let trade = &partial.trades[0];

    assert_eq!(trade.gross_r, Decimal::new(5, 1));
    assert_eq!(trade.fees_r, Decimal::new(2, 1));
    assert_eq!(trade.slippage_r, Decimal::new(1, 1));
    assert_eq!(trade.net_r, Decimal::new(2, 1));

    let legacy_release_id = Uuid::from_u128(59);
    let mut legacy_input = run_input(legacy_release_id);
    legacy_input.fees.per_execution_r = Decimal::new(1, 1);
    legacy_input.slippage.per_execution_r = Decimal::new(5, 2);
    let legacy = BacktestReplay::new(legacy_input, profile(legacy_release_id))
        .run(
            &ScriptedEvaluator {
                release_id: legacy_release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 100, 121, 95, 110, false),
            ],
        )
        .unwrap();
    let legacy_trade = &legacy.trades[0];

    assert_eq!(legacy_trade.fees_r, Decimal::new(1, 1));
    assert_eq!(legacy_trade.slippage_r, Decimal::new(5, 2));
    assert_eq!(legacy_trade.net_r, Decimal::new(185, 2));
}

#[test]
fn independent_replay_sessions_can_share_each_frame() {
    let release_id = Uuid::from_u128(56);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let evaluator = ScriptedEvaluator {
        release_id,
        emit_at: vec![59_999],
        exit_at: vec![119_999],
        cancel_at: Vec::new(),
        limit_entry: false,
        fail_at: None,
    };
    let frames = vec![
        frame(0, 100, 100, 100, 100, false),
        frame(60_000, 100, 105, 95, 100, false),
        frame(120_000, 106, 107, 100, 105, false),
    ];
    let mut baseline = replay.start_session(&evaluator).unwrap();
    let mut candidate = replay.start_session(&evaluator).unwrap();

    for frame in &frames {
        baseline.process_frame(frame).unwrap();
        candidate.process_frame(frame).unwrap();
    }

    assert_eq!(
        baseline.finish(frames.len() as u64),
        candidate.finish(frames.len() as u64)
    );
}

#[test]
fn failed_replay_returns_no_partial_metrics_or_trades() {
    let release_id = Uuid::from_u128(5);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let error = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: Vec::new(),
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: Some(59_999),
            },
            vec![frame(0, 100, 100, 100, 100, false)],
        )
        .unwrap_err();

    assert!(error.partial_result().is_none());
}

#[test]
fn evaluator_directives_apply_on_later_frames() {
    let release_id = Uuid::from_u128(6);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: vec![119_999],
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 105, 95, 100, false),
                frame(120_000, 106, 107, 100, 105, false),
            ],
        )
        .unwrap();

    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.trades[0].exit_reason, "EXIT_AT_NEXT_OPEN");
    assert_eq!(result.trades[0].exit_time, 120_000);
    assert_eq!(result.trades[0].exit_price, Decimal::new(106, 0));
}

#[test]
fn stale_feedback_is_not_replayed_after_trade_close() {
    let release_id = Uuid::from_u128(7);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 100, 125, 95, 110, false),
                frame(180_000, 110, 111, 109, 110, false),
            ],
        )
        .unwrap();

    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.trades[0].exit_reason, "TAKE_PROFIT");
}

#[test]
fn closed_trade_preserves_signal_htf_context_for_tuning_metrics() {
    let release_id = Uuid::from_u128(77);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: Vec::new(),
                limit_entry: false,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 100, 125, 95, 110, false),
            ],
        )
        .unwrap();

    let context = result.trades[0].htf_context.as_ref().expect("htf context");
    assert_eq!(context.higher_timeframe, Some(Timeframe::M5));
    assert_eq!(context.alignment, HtfAlignment::Aligned);
    assert_eq!(context.source, "STRUCTURE");
}

#[test]
fn cancelled_pending_limit_signal_cannot_enter_later() {
    let release_id = Uuid::from_u128(8);
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let result = replay
        .run(
            &ScriptedEvaluator {
                release_id,
                emit_at: vec![59_999],
                exit_at: Vec::new(),
                cancel_at: vec![119_999],
                limit_entry: true,
                fail_at: None,
            },
            vec![
                frame(0, 100, 100, 100, 100, false),
                frame(60_000, 105, 106, 104, 105, false),
                frame(120_000, 100, 101, 99, 100, false),
            ],
        )
        .unwrap();

    assert!(result.executions.is_empty());
    assert!(result.trades.is_empty());
}

#[test]
fn spread_replay_uses_actual_risk_and_partial_weighted_quotes_without_ledger_charge() {
    use backtest_engine::{CandlePriceReference, ExecutionKind, SpreadConfig};
    let release_id = Uuid::now_v7();
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id))
        .with_spread(SpreadConfig {
            price: Decimal::new(6, 1),
            candle_reference: CandlePriceReference::Mid,
        })
        .unwrap();
    let result = replay
        .run(
            &PartialExitEvaluator { release_id },
            vec![
                frame(0, 100, 101, 99, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 108, 112, 107, 111, false),
                frame(180_000, 115, 119, 114, 118, false),
            ],
        )
        .unwrap();
    assert_eq!(result.spread.price, Decimal::new(6, 1));
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.entry_price, Decimal::new(1003, 1));
    assert_eq!(trade.initial_risk_price, Some(Decimal::new(103, 1)));
    assert_eq!(trade.fees_r, Decimal::ZERO);
    assert_eq!(trade.slippage_r, Decimal::ZERO);
    assert_eq!(trade.net_r, trade.gross_r);
    assert_eq!(trade.realized_r, Decimal::new(5, 1));
    let expected_runner = (Decimal::new(5, 1) * (Decimal::new(118, 0) - trade.entry_price)
        / Decimal::new(103, 1))
    .round_dp(6);
    assert_eq!(
        trade.net_r,
        (Decimal::new(5, 1) + expected_runner).round_dp(6)
    );
    assert_eq!(
        result
            .executions
            .iter()
            .filter(|e| e.kind == ExecutionKind::PartialTakeProfit)
            .count(),
        1
    );
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(encoded["spread"]["candleReference"], "MID");
}

#[test]
fn explicit_zero_spread_is_identical_to_legacy_replay() {
    use backtest_engine::SpreadConfig;
    let release_id = Uuid::now_v7();
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id));
    let frames = vec![
        frame(0, 100, 101, 99, 100, false),
        frame(60_000, 100, 101, 99, 100, false),
        frame(120_000, 108, 112, 107, 111, false),
        frame(180_000, 115, 119, 114, 118, false),
    ];
    let evaluator = PartialExitEvaluator { release_id };
    let legacy = replay.run(&evaluator, frames.clone()).unwrap();
    let explicit = replay
        .with_spread(SpreadConfig::default())
        .unwrap()
        .run(&evaluator, frames)
        .unwrap();
    assert_eq!(legacy, explicit);
    let encoded = serde_json::to_value(&legacy).unwrap();
    let restored: SpreadConfig = serde_json::from_value(encoded["spread"].clone()).unwrap();
    assert_eq!(restored, SpreadConfig::default());
}

struct NonHalfPartialEvaluator {
    release_id: Uuid,
}

impl PlanEvaluator for NonHalfPartialEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }
    fn algorithm_version(&self) -> &'static str {
        "fixture-algo-v1"
    }
    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &analysis_domain::CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        for item in feedback {
            if let PlanFeedback::PartialTargetFilled {
                realized_fraction,
                remaining_fraction,
                ..
            } = item
            {
                assert_eq!(*realized_fraction, Decimal::new(3, 1));
                assert_eq!(*remaining_fraction, Decimal::new(7, 1));
            }
        }
        let mut evaluation = PartialExitEvaluator {
            release_id: self.release_id,
        }
        .evaluate(profile, snapshot, state, feedback)?;
        if let Some(signal) = &mut evaluation.signal {
            signal.execution_plan.as_mut().unwrap().tp1.fraction = Decimal::new(3, 1);
            evaluation.next_state.active_signal = Some(signal.clone());
        }
        Ok(evaluation)
    }
}

#[test]
fn partial_feedback_matches_configured_fraction_in_quote_replay() {
    use backtest_engine::{CandlePriceReference, SpreadConfig};
    let release_id = Uuid::now_v7();
    let replay = BacktestReplay::new(run_input(release_id), profile(release_id))
        .with_spread(SpreadConfig {
            price: Decimal::new(6, 1),
            candle_reference: CandlePriceReference::Mid,
        })
        .unwrap();
    let result = replay
        .run(
            &NonHalfPartialEvaluator { release_id },
            vec![
                frame(0, 100, 101, 99, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 108, 112, 107, 111, false),
                frame(180_000, 115, 119, 114, 118, false),
            ],
        )
        .unwrap();
    assert!(result.trades[0].partial_filled);
    assert_eq!(result.trades[0].realized_r, Decimal::new(3, 1));
}

struct CancellationRecoveryEvaluator {
    release_id: Uuid,
}
impl PlanEvaluator for CancellationRecoveryEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }
    fn algorithm_version(&self) -> &'static str {
        "fixture-algo-v1"
    }
    fn evaluate(
        &self,
        _profile: &MaterializedProfile,
        snapshot: &analysis_domain::CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        let mut next_state = state.clone();
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        let time = snapshot.evaluated_closed_candle_time;
        let candidate = match time {
            59_999 => {
                let mut invalid = signal(self.release_id, time);
                invalid.primary_target = Decimal::new(1002, 1); // Ask entry would already cross it.
                Some(invalid)
            }
            119_999 => {
                assert!(
                    state.active_signal.is_none(),
                    "cancelled entry must release the strategy slot"
                );
                assert_eq!(state.trade_state, TradeState::NotEntered);
                assert_eq!(state.setup_state, SetupState::Neutral);
                Some(signal(self.release_id, time))
            }
            _ => None,
        };
        if let Some(signal) = &candidate {
            next_state.active_signal = Some(signal.clone());
        }
        Ok(PlanEvaluation {
            next_state,
            signal: candidate,
            events: vec![],
            directives: vec![],
            rejected_candidates: vec![],
        })
    }
}

#[test]
fn cancelled_quote_entry_releases_the_slot_and_allows_the_next_valid_signal() {
    use backtest_engine::{CandlePriceReference, ExecutionKind, SpreadConfig};
    let release_id = Uuid::now_v7();
    let result = BacktestReplay::new(run_input(release_id), profile(release_id))
        .with_spread(SpreadConfig {
            price: Decimal::new(6, 1),
            candle_reference: CandlePriceReference::Mid,
        })
        .unwrap()
        .run(
            &CancellationRecoveryEvaluator { release_id },
            vec![
                frame(0, 100, 101, 99, 100, false),
                frame(60_000, 100, 101, 99, 100, false),
                frame(120_000, 100, 101, 99, 100, false),
                frame(180_000, 115, 121, 114, 120, false),
            ],
        )
        .unwrap();
    assert_eq!(
        result.executions.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            ExecutionKind::EntryCancelled,
            ExecutionKind::Entry,
            ExecutionKind::TakeProfit
        ]
    );
    assert_eq!(result.trades.len(), 1); // No fictitious trade from the rejected entry.
    assert_eq!(result.trades[0].entry_time, 120_000);
}

/// Signals whenever its slot is free and leaves expiry to execution, as mean reversion does.
struct ExpiringSignalEvaluator {
    release_id: Uuid,
}
impl PlanEvaluator for ExpiringSignalEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }
    fn algorithm_version(&self) -> &'static str {
        "fixture-algo-v1"
    }
    fn evaluate(
        &self,
        _profile: &MaterializedProfile,
        snapshot: &analysis_domain::CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        let mut next_state = state.clone();
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        let candidate = next_state.active_signal.is_none().then(|| {
            let time = snapshot.evaluated_closed_candle_time;
            let mut candidate = signal(self.release_id, time);
            candidate.expires_at = Some(time + 1 + 60_000);
            candidate
        });
        if let Some(candidate) = &candidate {
            next_state.active_signal = Some(candidate.clone());
        }
        Ok(PlanEvaluation {
            next_state,
            signal: candidate,
            events: vec![],
            directives: vec![],
            rejected_candidates: vec![],
        })
    }
}

#[test]
fn signal_expired_across_a_session_gap_releases_the_slot() {
    use backtest_engine::ExecutionKind;
    let release_id = Uuid::now_v7();
    let result = BacktestReplay::new(run_input(release_id), profile(release_id))
        .run(
            &ExpiringSignalEvaluator { release_id },
            vec![
                frame(0, 100, 101, 99, 100, false),
                // The market is closed for two bars, so the first signal expires unfilled.
                frame(180_000, 100, 101, 99, 100, false),
                frame(240_000, 100, 101, 99, 100, false),
            ],
        )
        .unwrap();

    let entries: Vec<_> = result
        .executions
        .iter()
        .filter(|execution| execution.kind == ExecutionKind::Entry)
        .map(|execution| execution.time)
        .collect();
    assert_eq!(entries, vec![240_000]);
}
