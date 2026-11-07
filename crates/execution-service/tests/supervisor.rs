use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanKind, PlanProfileKey,
    PriceZone, SignalDefinition, SignalExecutionPlan, TargetLeg,
};
use async_trait::async_trait;
use execution_domain::{MarketCursor, MonitoringEvent, PositionStage, TrackingHealth};
use execution_service::{
    ActiveTradeSequence, DurableTransition, ExecutionError, ExecutionSupervisor, HealthTransition,
    SupervisorEvent, SupervisorStep, TrackerSnapshot, TrackerStore,
};
use market_domain::{AggregateTrade, Candle, MarketEvent, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct MemoryStore {
    commits: Mutex<Vec<DurableTransition>>,
    checkpoints: Mutex<Vec<MarketCursor>>,
    fail: Mutex<bool>,
    snapshot: Mutex<TrackerSnapshot>,
}

#[async_trait]
impl TrackerStore for MemoryStore {
    async fn load_active(&self, _symbol: &Symbol) -> Result<TrackerSnapshot, ExecutionError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn commit_transition(
        &self,
        transition: DurableTransition,
    ) -> Result<Option<MonitoringEvent>, ExecutionError> {
        if *self.fail.lock().unwrap() {
            return Err(ExecutionError::Storage {
                operation: "test commit",
                message: "injected".to_owned(),
            });
        }
        self.commits.lock().unwrap().push(transition);
        Ok(Some(MonitoringEvent {
            cursor: 1,
            event_id: Uuid::now_v7(),
            event_type: "test".to_owned(),
            entity_type: "test".to_owned(),
            entity_id: None,
            payload: json!({}),
            created_at: 1,
        }))
    }

    async fn checkpoint_cursor(&self, cursor: MarketCursor) -> Result<(), ExecutionError> {
        self.checkpoints.lock().unwrap().push(cursor);
        Ok(())
    }

    async fn mark_health(
        &self,
        _change: HealthTransition,
    ) -> Result<MonitoringEvent, ExecutionError> {
        unreachable!()
    }
}

#[tokio::test]
async fn processes_market_events_without_browser_subscriber() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();

    assert_eq!(
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
                10
            ))))
            .await
            .unwrap(),
        SupervisorStep::Committed
    );
    assert_eq!(store.commits.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn ordered_aggregate_trade_events_advance_cursor() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            11,
        ))))
        .await
        .unwrap();

    let commits = store.commits.lock().unwrap();
    assert_eq!(
        commits[1].market_cursor,
        Some(MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 10,
            last_trade_time: 1_000
        })
    );
}

#[tokio::test]
async fn non_terminal_cursor_is_checkpointed_on_next_m1_close() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            11,
        ))))
        .await
        .unwrap();
    let candle = Candle::new(
        symbol(),
        Timeframe::M1,
        2_000,
        61_999,
        Decimal::new(4_105, 0),
        Decimal::new(4_120, 0),
        Decimal::new(4_100, 0),
        Decimal::new(4_110, 0),
    )
    .unwrap();

    supervisor
        .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(candle)))
        .await
        .unwrap();

    assert_eq!(
        store.checkpoints.lock().unwrap().as_slice(),
        &[MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 11,
            last_trade_time: 1_100,
        }]
    );
}

#[tokio::test]
async fn aggregate_trade_sequence_gap_is_rejected_for_exact_recovery() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();

    let step = supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            12,
        ))))
        .await
        .unwrap();

    assert_eq!(step, SupervisorStep::GapDetected);
    assert_eq!(store.commits.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn conservative_recovery_can_rebase_before_first_available_live_trade() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    assert_eq!(
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
                12
            ))))
            .await
            .unwrap(),
        SupervisorStep::GapDetected
    );

    assert_eq!(
        supervisor
            .rebase_market_cursor(MarketCursor {
                symbol: symbol(),
                last_processed_agg_trade_id: 11,
                last_trade_time: 1_199,
            })
            .await
            .unwrap(),
        SupervisorStep::Committed
    );
    assert_eq!(
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
                12
            ))))
            .await
            .unwrap(),
        SupervisorStep::Ignored
    );
    assert_eq!(
        store.checkpoints.lock().unwrap()[0].last_processed_agg_trade_id,
        11
    );
}

#[tokio::test]
async fn closed_m1_candle_reconciles_terminal_range_without_exact_tick() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    let candle = Candle::new(
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

    let step = supervisor
        .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(candle)))
        .await
        .unwrap();

    assert_eq!(step, SupervisorStep::Committed);
    let commits = store.commits.lock().unwrap();
    assert_eq!(commits.len(), 3);
    assert_eq!(
        commits[2]
            .virtual_trade_write
            .as_ref()
            .unwrap()
            .trade
            .trade_state,
        execution_domain::TradeState::Closed
    );
}

#[tokio::test]
async fn database_failure_freezes_symbol_until_recovery() {
    let store = Arc::new(MemoryStore::default());
    *store.fail.lock().unwrap() = true;
    let mut supervisor = ExecutionSupervisor::new(store);

    let step = supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();

    assert_eq!(step, SupervisorStep::Frozen);
    assert_eq!(supervisor.health(&symbol()), Some(TrackingHealth::Stale));
}

#[tokio::test]
async fn failed_partial_commit_rolls_back_runtime_and_retries_without_ghost_runner() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    let mut recommendation = signal();
    recommendation.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_095, 0),
        hard_stop: Decimal::new(4_090, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    let key = PlanProfileKey {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
    };

    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(recommendation)))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    let opened = store.commits.lock().unwrap()[1]
        .virtual_trade_write
        .as_ref()
        .unwrap()
        .trade
        .clone();
    *store.snapshot.lock().unwrap() = TrackerSnapshot {
        cursor: Some(MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 10,
            last_trade_time: 1_000,
        }),
        active_trades: vec![opened.clone()],
        active_trade_sequences: vec![ActiveTradeSequence {
            virtual_trade_id: opened.virtual_trade_id,
            next_sequence: 2,
        }],
        ..TrackerSnapshot::default()
    };
    *store.fail.lock().unwrap() = true;

    let failed = supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
            AggregateTrade::new(symbol(), 11, 11, 11, 1_100, 1_100, Decimal::new(4_120, 0))
                .unwrap(),
        )))
        .await
        .unwrap();

    assert_eq!(failed, SupervisorStep::Frozen);
    assert_eq!(
        supervisor
            .last_seen_cursor()
            .unwrap()
            .last_processed_agg_trade_id,
        10
    );
    let restored = supervisor.active_trade(&key).unwrap();
    assert_eq!(restored.position_stage, PositionStage::FullPosition);
    assert_eq!(restored.active_stop, Decimal::new(4_090, 0));

    let stale_directive = supervisor
        .process(SupervisorEvent::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: opened.signal_id,
            virtual_trade_id: Some(opened.virtual_trade_id),
            kind: DirectiveKind::MoveStop,
            effective_at: 1_101,
            price: Some(Decimal::new(4_095, 0)),
            reason: "must not mutate stale runtime".to_owned(),
            source_closed_candle_time: 1_099,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: None,
        }))
        .await
        .unwrap();

    assert_eq!(stale_directive, SupervisorStep::Frozen);
    assert_eq!(
        supervisor.active_trade(&key).unwrap().active_stop,
        Decimal::new(4_090, 0)
    );

    *store.fail.lock().unwrap() = false;
    supervisor.restore_active(&symbol()).await.unwrap();
    let retried = supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
            AggregateTrade::new(symbol(), 11, 11, 11, 1_100, 1_100, Decimal::new(4_120, 0))
                .unwrap(),
        )))
        .await
        .unwrap();

    assert_eq!(retried, SupervisorStep::Committed);
    let retry = store.commits.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        retry
            .trade_events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(retry.plan_feedback.len(), 1);
    assert_eq!(
        retry.virtual_trade_write.unwrap().trade.position_stage,
        PositionStage::Runner
    );
}

#[test]
fn only_one_aggregate_trade_processor_can_claim_a_symbol() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store);

    assert!(supervisor.claim_symbol(symbol()));
    assert!(!supervisor.claim_symbol(symbol()));
}

#[tokio::test]
async fn cancellation_event_stops_processing_without_commit() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());

    assert_eq!(
        supervisor.process(SupervisorEvent::Cancel).await.unwrap(),
        SupervisorStep::Cancelled
    );
    assert!(store.commits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn signal_and_directive_events_are_subscribed_inputs() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(signal())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    let trade_id = store.commits.lock().unwrap()[1]
        .virtual_trade_write
        .as_ref()
        .unwrap()
        .trade
        .virtual_trade_id;

    supervisor
        .process(SupervisorEvent::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: Uuid::nil(),
            virtual_trade_id: Some(trade_id),
            kind: DirectiveKind::ExitAtNextOpen,
            effective_at: 2_000,
            price: Some(Decimal::new(4_120, 0)),
            reason: "manual".to_owned(),
            source_closed_candle_time: 1_999,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: None,
        }))
        .await
        .unwrap();

    assert_eq!(store.commits.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn reverse_at_next_open_with_reverse_signal_opens_opposite_trade() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());
    let mut buy = signal();
    buy.plan = PlanKind::TrendBreakout;
    let mut sell = signal();
    sell.plan = PlanKind::TrendBreakout;
    sell.signal_id = Uuid::now_v7();
    sell.direction = Direction::Sell;
    sell.entry_zone = Some(PriceZone::new(Decimal::new(4_110, 0), Decimal::new(4_120, 0)).unwrap());
    sell.projected_entry = Some(Decimal::new(4_115, 0));
    sell.stop_loss = Decimal::new(4_130, 0);
    sell.primary_target = Decimal::new(4_090, 0);

    supervisor
        .process(SupervisorEvent::SignalOpened(Box::new(buy.clone())))
        .await
        .unwrap();
    supervisor
        .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(trade(
            10,
        ))))
        .await
        .unwrap();
    let trade_id = store.commits.lock().unwrap()[1]
        .virtual_trade_write
        .as_ref()
        .unwrap()
        .trade
        .virtual_trade_id;

    let step = supervisor
        .process(SupervisorEvent::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: buy.signal_id,
            virtual_trade_id: Some(trade_id),
            kind: DirectiveKind::ReverseAtNextOpen,
            effective_at: 2_000,
            price: Some(Decimal::new(4_115, 0)),
            reason: "TREND_VALID_OPPOSITE_BREAKOUT".to_owned(),
            source_closed_candle_time: 1_999,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: Some(sell),
        }))
        .await
        .unwrap();
    assert_eq!(step, SupervisorStep::Committed);
    let commits = store.commits.lock().unwrap();
    let last = commits.last().expect("reverse commit");
    assert_eq!(
        last.virtual_trade_write.as_ref().unwrap().trade.trade_state,
        execution_domain::TradeState::Closed
    );
    assert_eq!(last.virtual_trade_writes.len(), 1);
    assert_eq!(
        last.virtual_trade_writes[0].trade.trade_state,
        execution_domain::TradeState::Open
    );
    assert_eq!(
        last.virtual_trade_writes[0].trade.direction,
        Direction::Sell
    );
}

#[tokio::test]
async fn stale_directive_is_ignored_without_failing_supervisor() {
    let store = Arc::new(MemoryStore::default());
    let mut supervisor = ExecutionSupervisor::new(store.clone());

    let step = supervisor
        .process(SupervisorEvent::Directive(ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: Uuid::now_v7(),
            virtual_trade_id: None,
            kind: DirectiveKind::CancelEntry,
            effective_at: 2_000,
            price: None,
            reason: "stale".to_owned(),
            source_closed_candle_time: 1_999,
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            reverse_signal: None,
        }))
        .await
        .unwrap();

    assert_eq!(step, SupervisorStep::Ignored);
    assert!(store.commits.lock().unwrap().is_empty());
}

fn signal() -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at: 1_000,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(4_100, 0), Decimal::new(4_110, 0)).unwrap()),
        projected_entry: Some(Decimal::new(4_105, 0)),
        stop_loss: Decimal::new(4_090, 0),
        primary_target: Decimal::new(4_130, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
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

fn trade(id: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol(),
        id,
        id,
        id,
        id * 100,
        id * 100,
        Decimal::new(4_105, 0),
    )
    .unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}
