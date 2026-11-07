use analysis_domain::{
    Direction, ExecutionInstruction, PlanFeedback, PlanKind, PriceZone, SignalDefinition,
    SignalEvent, SignalEventKind, SignalExecutionPlan, TargetLeg, VersionIdentity,
};
use execution_domain::{
    Ambiguity, ExecutionModel, MarketCursor, PositionStage, ReconciliationSource, TrackingHealth,
    TradeHealth, TradeState, VirtualTrade, VirtualTradeEvent, VirtualTradeEventKind,
};
use execution_service::{
    DurableTransition, MonitoringStore, NewMonitoringEvent, NewScalpingAuditSnapshot,
    ScalpingAuditStore, TrackerState, TrackerStore, VirtualTradeHistoryQuery, VirtualTradeStore,
    VirtualTradeWrite, signal_recommendation_payload, virtual_trade_payload,
};
use execution_storage::{
    MIGRATOR, MonitoringRepository, ScalpingAuditRepository, TrackerRepository,
    VirtualTradeRepository,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::{Arc, OnceLock};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

const NOW_MS: i64 = 1_782_195_600_000;
static TEST_DB_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

async fn test_pool() -> (PgPool, OwnedMutexGuard<()>) {
    let guard = TEST_DB_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to an isolated test database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    MIGRATOR.run(&pool).await.expect("run migrations");
    sqlx::query(
        "TRUNCATE scalping_audit_snapshots, virtual_trade_events, virtual_trades, tracker_states, \
         market_event_cursors, monitoring_events, plan_feedback, trade_management_directives, signal_events, signals, \
         plan_runtime_states, analysis_evaluations, analysis_profiles, configuration_releases, \
         markets RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate phase 3 tables");
    sqlx::query(
        "INSERT INTO markets \
         (symbol, enabled, status, price_tick, onboard_time, session_market) \
         VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
    )
    .execute(&pool)
    .await
    .unwrap();
    (pool, guard)
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn signal(signal_id: Uuid, release_id: Uuid) -> SignalDefinition {
    SignalDefinition {
        signal_id,
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
        evaluated_at: NOW_MS,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(PriceZone::new(Decimal::new(4100, 0), Decimal::new(4101, 0)).unwrap()),
        projected_entry: None,
        stop_loss: Decimal::new(4090, 0),
        primary_target: Decimal::new(4120, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::TWO,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: None,
        effective_at: NOW_MS + 1,
        detected_at: NOW_MS + 1,
        published_at: NOW_MS + 1,
        release_id,
        algorithm_version: "ict-v1".to_owned(),
        parameter_version: "ict-1m-v1".to_owned(),
        created_at: NOW_MS,
        expires_at: None,
    }
}

async fn seed_signal(pool: &PgPool, signal_id: Uuid, release_id: Uuid) {
    seed_signal_definition(pool, signal(signal_id, release_id)).await;
}

async fn seed_signal_definition(pool: &PgPool, definition: SignalDefinition) {
    sqlx::query(
        "INSERT INTO signals \
         (signal_id, symbol, plan, interval, definition, evidence, release_id, \
          algorithm_version, parameter_version, created_at) \
         VALUES ($1, 'XAUUSDT', 'ICT', '1m', $2, '[]', $3, 'ict-v1', 'ict-1m-v1', NOW())",
    )
    .bind(definition.signal_id)
    .bind(serde_json::to_value(&definition).unwrap())
    .bind(definition.release_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_history_trade(
    pool: &PgPool,
    tracker: &TrackerRepository,
    signal_id: Uuid,
    virtual_trade_id: Uuid,
    release_id: Uuid,
    actual_entry_time: i64,
) {
    let mut seeded_trade = trade(virtual_trade_id, signal_id, release_id);
    seeded_trade.actual_entry_time = actual_entry_time;
    seed_history_trade_record(pool, tracker, seeded_trade).await;
}

async fn seed_history_trade_record(
    pool: &PgPool,
    tracker: &TrackerRepository,
    seeded_trade: VirtualTrade,
) {
    seed_signal(
        pool,
        seeded_trade.signal_id,
        seeded_trade.versions.release_id,
    )
    .await;
    let virtual_trade_id = seeded_trade.virtual_trade_id;
    tracker
        .commit_transition(DurableTransition {
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: seeded_trade,
            }),
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "VIRTUAL_TRADE_OPENED".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(virtual_trade_id),
                payload: json!({"virtualTradeId": virtual_trade_id}),
            }],
            ..DurableTransition::default()
        })
        .await
        .unwrap();
}

fn virtual_trade_history_query(cursor: Option<i64>, limit: u32) -> VirtualTradeHistoryQuery {
    VirtualTradeHistoryQuery {
        symbol: Some("XAUUSDT".to_owned()),
        plan: None,
        timeframe: None,
        direction: None,
        exit_reason: None,
        from: None,
        to: None,
        cursor,
        limit,
    }
}

fn virtual_trade_history_ids(items: &[serde_json::Value]) -> Vec<Uuid> {
    items
        .iter()
        .map(|item| {
            Uuid::parse_str(
                item["virtualTradeId"]
                    .as_str()
                    .expect("history item virtual trade id"),
            )
            .unwrap()
        })
        .collect()
}

fn trade(virtual_trade_id: Uuid, signal_id: Uuid, release_id: Uuid) -> VirtualTrade {
    VirtualTrade {
        virtual_trade_id,
        signal_id,
        symbol: symbol(),
        plan: PlanKind::Ict,
        timeframe: Timeframe::M1,
        direction: Direction::Buy,
        trade_state: TradeState::Open,
        position_stage: PositionStage::Legacy,
        trade_health: TradeHealth::Healthy,
        actual_entry_time: NOW_MS + 1,
        actual_entry_price: Decimal::new(4100, 0),
        exit_time: None,
        exit_price: None,
        primary_target: Decimal::new(4120, 0),
        reference_levels: vec![Decimal::new(4110, 0)],
        initial_stop: Decimal::new(4090, 0),
        active_stop: Decimal::new(4090, 0),
        thesis_invalidation: None,
        tp1_price: None,
        tp1_r: None,
        tp1_fraction: None,
        tp1_filled_at: None,
        remaining_fraction: Decimal::ONE,
        realized_fraction: Decimal::ZERO,
        realized_r: Decimal::ZERO,
        tp2_zone_low: None,
        tp2_zone_high: None,
        last_management_effective_at: None,
        last_management_candle_time: None,
        versions: VersionIdentity {
            release_id,
            algorithm_version: "ict-v1".to_owned(),
            parameter_version: "ict-1m-v1".to_owned(),
        },
        execution_model: ExecutionModel::LiveTrade,
        reconciliation_source: None,
        ambiguity: Ambiguity::None,
        gross_r: None,
        mfe_r: Decimal::ZERO,
        mae_r: Decimal::ZERO,
        duration_ms: None,
        exit_reason: None,
    }
}

fn runner_trade(virtual_trade_id: Uuid, signal_id: Uuid, release_id: Uuid) -> VirtualTrade {
    VirtualTrade {
        position_stage: PositionStage::Runner,
        active_stop: Decimal::new(4_100, 0),
        trade_health: TradeHealth::Weakening,
        thesis_invalidation: Some(Decimal::new(4_092, 0)),
        tp1_price: Some(Decimal::new(4_110, 0)),
        tp1_r: Some(Decimal::ONE),
        tp1_fraction: Some(Decimal::new(5, 1)),
        tp1_filled_at: Some(NOW_MS + 2),
        remaining_fraction: Decimal::new(5, 1),
        realized_fraction: Decimal::new(5, 1),
        realized_r: Decimal::new(5, 1),
        tp2_zone_low: Some(Decimal::new(4_128, 0)),
        tp2_zone_high: Some(Decimal::new(4_130, 0)),
        last_management_effective_at: Some(NOW_MS + 3),
        last_management_candle_time: Some(NOW_MS + 2),
        primary_target: Decimal::new(4_128, 0),
        ..trade(virtual_trade_id, signal_id, release_id)
    }
}

fn assert_lifecycle_payload_parity(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    source: &str,
) {
    for key in [
        "positionStage",
        "remainingFraction",
        "realizedFraction",
        "realizedR",
        "tradeHealth",
        "thesisInvalidation",
        "hardStop",
        "activeStop",
        "tp1Price",
        "tp1R",
        "tp1Fraction",
        "tp1Filled",
        "tp1FilledAt",
        "tp2Zone",
        "tp2ExecutionEdge",
        "analyticalTarget",
        "ambiguity",
        "reconciliationSource",
    ] {
        assert_eq!(actual.get(key), expected.get(key), "{source}.{key}");
    }
}

fn transition(signal_id: Uuid, release_id: Uuid) -> DurableTransition {
    let virtual_trade_id = Uuid::now_v7();
    DurableTransition {
        tracker_state: Some(TrackerState {
            signal_id,
            symbol: symbol(),
            state: json!({"phase": "entered"}),
            tracking_health: TrackingHealth::Live,
        }),
        virtual_trade_write: Some(VirtualTradeWrite {
            trade: trade(virtual_trade_id, signal_id, release_id),
        }),
        trade_events: vec![VirtualTradeEvent {
            event_id: Uuid::now_v7(),
            virtual_trade_id,
            sequence: 0,
            kind: VirtualTradeEventKind::EntryTriggered,
            event_time: NOW_MS + 1,
            payload: json!({"price": "4100"}),
        }],
        signal_events: vec![SignalEvent {
            event_id: Uuid::now_v7(),
            signal_id,
            sequence: 0,
            kind: SignalEventKind::Triggered,
            effective_at: NOW_MS + 1,
            recorded_at: NOW_MS + 1,
            virtual_trade_id: Some(virtual_trade_id),
            price: Some(Decimal::new(4100, 0)),
            reason: None,
        }],
        market_cursor: Some(MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 100,
            last_trade_time: NOW_MS + 1,
        }),
        monitoring_events: vec![NewMonitoringEvent {
            event_id: Uuid::now_v7(),
            event_type: "VIRTUAL_TRADE_OPENED".to_owned(),
            entity_type: "VIRTUAL_TRADE".to_owned(),
            entity_id: Some(virtual_trade_id),
            payload: json!({"plan": "ICT"}),
        }],
        plan_feedback: vec![PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time: NOW_MS + 1,
            price: Decimal::new(4100, 0),
        }],
        ..DurableTransition::default()
    }
}

fn partial_transition(trade: VirtualTrade, inject_failure_after_writes: bool) -> DurableTransition {
    let virtual_trade_id = trade.virtual_trade_id;
    let signal_id = trade.signal_id;
    DurableTransition {
        tracker_state: Some(TrackerState {
            signal_id,
            symbol: trade.symbol.clone(),
            state: json!({
                "tradeState": "OPEN",
                "positionStage": "RUNNER",
                "remainingFraction": "0.5",
                "realizedFraction": "0.5",
                "realizedR": "0.5",
            }),
            tracking_health: TrackingHealth::Live,
        }),
        virtual_trade_write: Some(VirtualTradeWrite {
            trade: trade.clone(),
        }),
        trade_events: vec![
            VirtualTradeEvent {
                event_id: Uuid::now_v7(),
                virtual_trade_id,
                sequence: 1,
                kind: VirtualTradeEventKind::PartialTargetTouched,
                event_time: NOW_MS + 2,
                payload: json!({"price": "4110", "realizedFraction": "0.5"}),
            },
            VirtualTradeEvent {
                event_id: Uuid::now_v7(),
                virtual_trade_id,
                sequence: 2,
                kind: VirtualTradeEventKind::RunnerActivated,
                event_time: NOW_MS + 2,
                payload: json!({"activeStop": "4100", "remainingFraction": "0.5"}),
            },
        ],
        plan_feedback: vec![PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            time: NOW_MS + 2,
            price: Decimal::new(4_110, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        }],
        monitoring_events: vec![NewMonitoringEvent {
            event_id: Uuid::now_v7(),
            event_type: "VIRTUAL_TRADE_PARTIAL_TARGET_FILLED".to_owned(),
            entity_type: "VIRTUAL_TRADE".to_owned(),
            entity_id: Some(virtual_trade_id),
            payload: json!({"virtualTradeId": virtual_trade_id, "positionStage": "RUNNER"}),
        }],
        inject_failure_after_writes,
        ..DurableTransition::default()
    }
}

#[tokio::test]
async fn tracker_transition_is_atomic_and_writes_cursor_and_outbox_monotonically() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;

    let mut rollback = transition(signal_id, release_id);
    rollback.inject_failure_after_writes = true;
    assert!(repository.commit_transition(rollback).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM virtual_trades")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let feedback_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_feedback")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(feedback_count, 0);

    let outbox = repository
        .commit_transition(transition(signal_id, release_id))
        .await
        .unwrap()
        .expect("transition with a monitoring event must write an outbox row");
    assert!(outbox.cursor > 0);
    let snapshot = repository.load_active(&symbol()).await.unwrap();
    assert_eq!(snapshot.active_trades.len(), 1);
    assert_eq!(snapshot.cursor.unwrap().last_processed_agg_trade_id, 100);
    let feedback_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_feedback")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(feedback_count, 1);

    let duplicate = repository
        .commit_transition(transition(signal_id, release_id))
        .await;
    assert!(duplicate.is_err(), "one virtual trade per signal");

    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM monitoring_events WHERE published_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 1);
    pool.close().await;
}

#[tokio::test]
async fn partial_target_transition_is_atomic_idempotent_and_survives_restart() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;
    repository
        .commit_transition(transition(signal_id, release_id))
        .await
        .unwrap();

    let opened = repository
        .load_active(&symbol())
        .await
        .unwrap()
        .active_trades[0]
        .clone();
    let runner = runner_trade(opened.virtual_trade_id, signal_id, release_id);
    assert!(
        repository
            .commit_transition(partial_transition(runner.clone(), true))
            .await
            .is_err()
    );

    let after_rollback = repository.load_active(&symbol()).await.unwrap();
    assert_eq!(
        after_rollback.active_trades[0].position_stage,
        PositionStage::Legacy
    );
    let partial_events_after_rollback: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM virtual_trade_events WHERE virtual_trade_id = $1 AND event_type IN ('PARTIAL_TARGET_TOUCHED', 'RUNNER_ACTIVATED')",
    )
    .bind(opened.virtual_trade_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(partial_events_after_rollback, 0);
    let partial_feedback_after_rollback: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plan_feedback WHERE virtual_trade_id = $1 AND feedback_type = 'PARTIAL_TARGET_FILLED'",
    )
    .bind(opened.virtual_trade_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(partial_feedback_after_rollback, 0);

    repository
        .commit_transition(partial_transition(runner.clone(), false))
        .await
        .unwrap();
    let restored = repository.load_active(&symbol()).await.unwrap();
    assert_eq!(restored.active_trades, vec![runner.clone()]);
    assert_eq!(restored.active_trade_sequences[0].next_sequence, 3);

    repository
        .commit_transition(DurableTransition {
            plan_feedback: vec![PlanFeedback::PartialTargetFilled {
                signal_id,
                virtual_trade_id: runner.virtual_trade_id,
                time: NOW_MS + 2,
                price: Decimal::new(4_110, 0),
                realized_fraction: Decimal::new(5, 1),
                remaining_fraction: Decimal::new(5, 1),
            }],
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "VIRTUAL_TRADE_PARTIAL_TARGET_REPLAYED".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(runner.virtual_trade_id),
                payload: json!({"virtualTradeId": runner.virtual_trade_id}),
            }],
            ..DurableTransition::default()
        })
        .await
        .unwrap();
    let feedback_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plan_feedback WHERE virtual_trade_id = $1 AND feedback_type = 'PARTIAL_TARGET_FILLED'",
    )
    .bind(runner.virtual_trade_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(feedback_count, 1);
    pool.close().await;
}

#[tokio::test]
async fn tp1_fraction_constraint_preserves_legacy_null_and_rejects_non_half_runner() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let release_id = Uuid::now_v7();

    let legacy_signal_id = Uuid::now_v7();
    seed_signal(&pool, legacy_signal_id, release_id).await;
    repository
        .commit_transition(transition(legacy_signal_id, release_id))
        .await
        .expect("legacy trade with NULL TP1 fraction remains valid");

    let runner_signal_id = Uuid::now_v7();
    seed_signal(&pool, runner_signal_id, release_id).await;
    let mut invalid_runner = runner_trade(Uuid::now_v7(), runner_signal_id, release_id);
    invalid_runner.tp1_fraction = Some(Decimal::new(4, 1));
    let result = repository
        .commit_transition(DurableTransition {
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: invalid_runner.clone(),
            }),
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "INVALID_RUNNER_FIXTURE".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(invalid_runner.virtual_trade_id),
                payload: json!({"tp1Fraction": "0.4"}),
            }],
            ..DurableTransition::default()
        })
        .await;

    assert!(result.is_err(), "runner TP1 fraction 0.4 must be rejected");
    let persisted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM virtual_trades WHERE signal_id = $1")
            .bind(runner_signal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(persisted, 0);
    pool.close().await;
}

#[tokio::test]
async fn moved_target_survives_repository_reload() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;
    repository
        .commit_transition(transition(signal_id, release_id))
        .await
        .unwrap();

    let snapshot = repository.load_active(&symbol()).await.unwrap();
    let mut moved = snapshot.active_trades[0].clone();
    moved.primary_target = Decimal::new(4_150, 0);
    repository
        .commit_transition(DurableTransition {
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: moved.clone(),
            }),
            trade_events: vec![VirtualTradeEvent {
                event_id: Uuid::now_v7(),
                virtual_trade_id: moved.virtual_trade_id,
                sequence: 1,
                kind: VirtualTradeEventKind::TargetMoved,
                event_time: NOW_MS + 2,
                payload: json!({"primaryTarget": moved.primary_target}),
            }],
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "VIRTUAL_TRADE_TARGET_MOVED".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(moved.virtual_trade_id),
                payload: json!({
                    "virtualTradeId": moved.virtual_trade_id,
                    "primaryTarget": moved.primary_target
                }),
            }],
            ..DurableTransition::default()
        })
        .await
        .unwrap();

    let restored = repository.load_active(&symbol()).await.unwrap();
    assert_eq!(
        restored.active_trades[0].primary_target,
        moved.primary_target
    );
    pool.close().await;
}

#[tokio::test]
async fn runner_trade_round_trips_through_tracker_history_and_monitoring() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let monitoring = MonitoringRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    let virtual_trade_id = Uuid::now_v7();
    let mut definition = signal(signal_id, release_id);
    definition.stop_loss = Decimal::new(4_085, 0);
    definition.primary_target = Decimal::new(4_130, 0);
    definition.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_092, 0),
        hard_stop: Decimal::new(4_085, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    seed_signal_definition(&pool, definition).await;
    let mut expected = runner_trade(virtual_trade_id, signal_id, release_id);
    expected.initial_stop = Decimal::new(4_085, 0);
    expected.primary_target = Decimal::new(4_130, 0);

    tracker
        .commit_transition(DurableTransition {
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: expected.clone(),
            }),
            trade_events: vec![
                VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id,
                    sequence: 0,
                    kind: VirtualTradeEventKind::PartialTargetTouched,
                    event_time: NOW_MS + 2,
                    payload: json!({"price": "4110"}),
                },
                VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id,
                    sequence: 1,
                    kind: VirtualTradeEventKind::RunnerActivated,
                    event_time: NOW_MS + 2,
                    payload: json!({"remainingFraction": "0.5"}),
                },
            ],
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "VIRTUAL_TRADE_UPDATED".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(virtual_trade_id),
                payload: json!({"virtualTradeId": virtual_trade_id}),
            }],
            ..DurableTransition::default()
        })
        .await
        .unwrap();

    let restored = tracker.load_active(&symbol()).await.unwrap();
    assert_eq!(restored.active_trades, vec![expected.clone()]);

    let page = history
        .history(VirtualTradeHistoryQuery {
            symbol: Some("XAUUSDT".to_owned()),
            plan: None,
            timeframe: None,
            direction: None,
            exit_reason: None,
            from: None,
            to: None,
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    let history_trade = &page.items[0];
    let expected_payload = virtual_trade_payload(&expected);
    assert_eq!(expected_payload["hardStop"], "4085");
    assert_eq!(expected_payload["tp1Filled"], true);
    assert_eq!(expected_payload["tp2Zone"]["low"], "4128");
    assert_eq!(expected_payload["analyticalTarget"], "4130");
    assert_lifecycle_payload_parity(&expected_payload, history_trade, "history");
    assert_eq!(history_trade["positionStage"], "RUNNER");
    assert_eq!(history_trade["tradeHealth"], "WEAKENING");
    assert_eq!(history_trade["remainingFraction"], "0.5");
    assert_eq!(history_trade["realizedR"], "0.5");
    assert_eq!(history_trade["tp1Price"], "4110");
    assert_eq!(history_trade["thesisInvalidation"], "4092");
    assert_eq!(history_trade["tp2ZoneLow"], "4128");
    assert_eq!(history_trade["tp2ZoneHigh"], "4130");

    let snapshot = monitoring.snapshot().await.unwrap();
    let monitored = &snapshot.virtual_trades[0];
    assert_lifecycle_payload_parity(&expected_payload, monitored, "snapshot");
    assert_eq!(monitored["positionStage"], "RUNNER");
    assert_eq!(monitored["tradeHealth"], "WEAKENING");
    assert_eq!(monitored["remainingFraction"], "0.5");
    assert_eq!(monitored["realizedR"], "0.5");
    assert_eq!(monitored["tp1Price"], "4110");
    assert_eq!(monitored["thesisInvalidation"], "4092");
    assert_eq!(monitored["tp2ZoneLow"], "4128");
    assert_eq!(monitored["tp2ZoneHigh"], "4130");
    assert_eq!(monitored["hardStop"], "4085");
    assert_eq!(monitored["activeStop"], "4100");
    assert_eq!(monitored["tp1Fraction"], "0.5");
    assert_eq!(monitored["tp1Filled"], true);
    assert_eq!(monitored["tp1FilledAt"], NOW_MS + 2);
    assert_eq!(monitored["tp2Zone"]["low"], "4128");
    assert_eq!(monitored["tp2Zone"]["high"], "4130");
    assert_eq!(monitored["tp2ExecutionEdge"], "4128");
    assert_eq!(monitored["analyticalTarget"], "4130");
    assert_eq!(monitored["ambiguity"], "NONE");
    assert!(monitored["reconciliationSource"].is_null());
    let detail = history
        .detail(virtual_trade_id)
        .await
        .unwrap()
        .expect("trade detail");
    assert_lifecycle_payload_parity(&expected_payload, &detail.trade, "detail");
    let event_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM virtual_trade_events \
         WHERE virtual_trade_id = $1 ORDER BY sequence",
    )
    .bind(virtual_trade_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(event_kinds, ["PARTIAL_TARGET_TOUCHED", "RUNNER_ACTIVATED"]);
    assert!(
        sqlx::query(
            "UPDATE virtual_trades SET position_stage = 'UNKNOWN' WHERE virtual_trade_id = $1"
        )
        .bind(virtual_trade_id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE virtual_trades SET remaining_fraction = 0.6 WHERE virtual_trade_id = $1"
        )
        .bind(virtual_trade_id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE virtual_trades SET tp2_zone_low = 4131 WHERE virtual_trade_id = $1")
            .bind(virtual_trade_id)
            .execute(&pool)
            .await
            .is_err()
    );
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_keeps_timestamp_ties_across_cursor_pages() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::parse_str("00000000-0000-0000-0000-000000000100").unwrap();
    let signal_ids = [
        Uuid::parse_str("00000000-0000-0000-0000-000000000101").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000102").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000103").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000104").unwrap(),
    ];
    let trade_ids = [
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
    ];

    for (index, (signal_id, trade_id)) in signal_ids.iter().zip(trade_ids).enumerate() {
        seed_history_trade(
            &pool,
            &tracker,
            *signal_id,
            trade_id,
            release_id,
            if index == 0 { NOW_MS } else { NOW_MS + 1 },
        )
        .await;
    }

    let first_page = history
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();
    let repeated_first_page = history
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();
    assert_eq!(first_page.items, repeated_first_page.items);
    assert_eq!(
        first_page.items.len(),
        3,
        "the complete timestamp tie expands the page"
    );
    assert_eq!(first_page.next_cursor, Some(NOW_MS + 1));

    let second_page = history
        .history(virtual_trade_history_query(first_page.next_cursor, 2))
        .await
        .unwrap();
    assert_eq!(second_page.items.len(), 1);
    assert_eq!(second_page.next_cursor, None);
    let paged_items = first_page
        .items
        .iter()
        .chain(&second_page.items)
        .cloned()
        .collect::<Vec<_>>();
    let paged_ids = virtual_trade_history_ids(&paged_items);

    assert_eq!(
        paged_ids,
        vec![trade_ids[3], trade_ids[2], trade_ids[1], trade_ids[0]]
    );
    let unique_ids = paged_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique_ids.len(), paged_ids.len());
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_exact_limit_cursor_only_exists_when_rows_remain() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::from_u128(200);

    for offset in 0..3_u128 {
        seed_history_trade(
            &pool,
            &tracker,
            Uuid::from_u128(201 + offset),
            Uuid::from_u128(11 + offset),
            release_id,
            NOW_MS + i64::try_from(offset).unwrap(),
        )
        .await;
    }

    let first_page = history
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 2);
    assert_eq!(first_page.next_cursor, Some(NOW_MS + 1));

    let last_page = history
        .history(virtual_trade_history_query(first_page.next_cursor, 2))
        .await
        .unwrap();
    assert_eq!(last_page.items.len(), 1);
    assert_eq!(last_page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_exact_limit_without_older_rows_has_no_cursor() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::from_u128(300);

    for offset in 0..2_u128 {
        seed_history_trade(
            &pool,
            &tracker,
            Uuid::from_u128(301 + offset),
            Uuid::from_u128(21 + offset),
            release_id,
            NOW_MS + i64::try_from(offset).unwrap(),
        )
        .await;
    }

    let only_page = history
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();
    assert_eq!(only_page.items.len(), 2);
    assert_eq!(only_page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_empty_result_has_no_cursor() {
    let (pool, _guard) = test_pool().await;
    let page = VirtualTradeRepository::new(pool.clone())
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();

    assert!(page.items.is_empty());
    assert_eq!(page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_all_timestamp_ties_expand_without_cursor() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::from_u128(400);
    let trade_ids = [
        Uuid::from_u128(31),
        Uuid::from_u128(32),
        Uuid::from_u128(33),
    ];

    for (offset, trade_id) in trade_ids.into_iter().enumerate() {
        seed_history_trade(
            &pool,
            &tracker,
            Uuid::from_u128(401 + u128::try_from(offset).unwrap()),
            trade_id,
            release_id,
            NOW_MS,
        )
        .await;
    }

    let page = history
        .history(virtual_trade_history_query(None, 2))
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        3,
        "timestamp ties make limit a soft limit"
    );
    assert_eq!(
        virtual_trade_history_ids(&page.items),
        vec![trade_ids[2], trade_ids[1], trade_ids[0]]
    );
    assert_eq!(page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_cursor_page_expands_its_boundary_ties() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::from_u128(500);
    let fixtures = [
        (Uuid::from_u128(41), NOW_MS + 2),
        (Uuid::from_u128(42), NOW_MS + 2),
        (Uuid::from_u128(43), NOW_MS + 2),
        (Uuid::from_u128(44), NOW_MS + 1),
        (Uuid::from_u128(45), NOW_MS + 3),
    ];

    for (offset, (trade_id, actual_entry_time)) in fixtures.into_iter().enumerate() {
        seed_history_trade(
            &pool,
            &tracker,
            Uuid::from_u128(501 + u128::try_from(offset).unwrap()),
            trade_id,
            release_id,
            actual_entry_time,
        )
        .await;
    }

    let page = history
        .history(virtual_trade_history_query(Some(NOW_MS + 3), 2))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(
        virtual_trade_history_ids(&page.items),
        vec![fixtures[2].0, fixtures[1].0, fixtures[0].0]
    );
    assert_eq!(page.next_cursor, Some(NOW_MS + 2));

    let last_page = history
        .history(virtual_trade_history_query(page.next_cursor, 2))
        .await
        .unwrap();
    assert_eq!(
        virtual_trade_history_ids(&last_page.items),
        vec![fixtures[3].0]
    );
    assert_eq!(last_page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_filters_preserve_expected_rows() {
    let (pool, _guard) = test_pool().await;
    let tracker = TrackerRepository::new(pool.clone());
    let history = VirtualTradeRepository::new(pool.clone());
    let release_id = Uuid::from_u128(600);
    let target_time = NOW_MS + 10;
    let target_id = Uuid::from_u128(1_001);
    let lower_id = Uuid::from_u128(1_002);
    let upper_id = Uuid::from_u128(1_003);
    let plan_id = Uuid::from_u128(1_004);
    let timeframe_id = Uuid::from_u128(1_005);
    let direction_id = Uuid::from_u128(1_006);
    let exit_reason_id = Uuid::from_u128(1_007);
    let mut fixtures = Vec::new();

    for (offset, (trade_id, actual_entry_time)) in [
        (target_id, target_time),
        (lower_id, target_time - 1),
        (upper_id, target_time + 1),
        (plan_id, target_time),
        (timeframe_id, target_time),
        (direction_id, target_time),
        (exit_reason_id, target_time),
    ]
    .into_iter()
    .enumerate()
    {
        let mut seeded_trade = trade(
            trade_id,
            Uuid::from_u128(601 + u128::try_from(offset).unwrap()),
            release_id,
        );
        seeded_trade.actual_entry_time = actual_entry_time;
        seeded_trade.exit_reason = Some("TARGET".to_owned());
        fixtures.push(seeded_trade);
    }
    fixtures[3].plan = PlanKind::Smc;
    fixtures[4].timeframe = Timeframe::M5;
    fixtures[5].direction = Direction::Sell;
    fixtures[6].exit_reason = Some("STOP".to_owned());

    for seeded_trade in fixtures {
        seed_history_trade_record(&pool, &tracker, seeded_trade).await;
    }

    let base_query = virtual_trade_history_query(None, 20);
    let cases = [
        (
            "from is inclusive",
            VirtualTradeHistoryQuery {
                from: Some(target_time),
                ..base_query.clone()
            },
            vec![
                upper_id,
                exit_reason_id,
                direction_id,
                timeframe_id,
                plan_id,
                target_id,
            ],
        ),
        (
            "to is inclusive",
            VirtualTradeHistoryQuery {
                to: Some(target_time),
                ..base_query.clone()
            },
            vec![
                exit_reason_id,
                direction_id,
                timeframe_id,
                plan_id,
                target_id,
                lower_id,
            ],
        ),
        (
            "plan",
            VirtualTradeHistoryQuery {
                plan: Some("ICT".to_owned()),
                ..base_query.clone()
            },
            vec![
                upper_id,
                exit_reason_id,
                direction_id,
                timeframe_id,
                target_id,
                lower_id,
            ],
        ),
        (
            "timeframe",
            VirtualTradeHistoryQuery {
                timeframe: Some("1m".to_owned()),
                ..base_query.clone()
            },
            vec![
                upper_id,
                exit_reason_id,
                direction_id,
                plan_id,
                target_id,
                lower_id,
            ],
        ),
        (
            "direction",
            VirtualTradeHistoryQuery {
                direction: Some("BUY".to_owned()),
                ..base_query.clone()
            },
            vec![
                upper_id,
                exit_reason_id,
                timeframe_id,
                plan_id,
                target_id,
                lower_id,
            ],
        ),
        (
            "exit reason",
            VirtualTradeHistoryQuery {
                exit_reason: Some("TARGET".to_owned()),
                ..base_query.clone()
            },
            vec![
                upper_id,
                direction_id,
                timeframe_id,
                plan_id,
                target_id,
                lower_id,
            ],
        ),
        (
            "combined",
            VirtualTradeHistoryQuery {
                plan: Some("ICT".to_owned()),
                timeframe: Some("1m".to_owned()),
                direction: Some("BUY".to_owned()),
                exit_reason: Some("TARGET".to_owned()),
                from: Some(target_time),
                to: Some(target_time),
                ..base_query
            },
            vec![target_id],
        ),
    ];

    for (name, query, expected_ids) in cases {
        let page = history.history(query).await.unwrap();
        assert_eq!(
            virtual_trade_history_ids(&page.items),
            expected_ids,
            "{name}"
        );
        assert_eq!(page.next_cursor, None, "{name}");
    }
    pool.close().await;
}

#[tokio::test]
async fn virtual_trade_history_has_symbol_timestamp_uuid_index() {
    let (pool, _guard) = test_pool().await;
    let index_definition = sqlx::query_scalar::<_, String>(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = current_schema() \
           AND tablename = 'virtual_trades' \
           AND indexname = 'virtual_trades_symbol_entry_history_idx'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap()
    .expect("virtual trade history index must exist");

    assert!(
        index_definition.contains("(symbol, actual_entry_time DESC, virtual_trade_id DESC)"),
        "unexpected index definition: {index_definition}"
    );
    pool.close().await;
}

#[tokio::test]
async fn legacy_trade_rows_load_runner_defaults_without_backfill_behavior() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    let virtual_trade_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;
    sqlx::query(
        "INSERT INTO virtual_trades \
         (virtual_trade_id, signal_id, symbol, plan, interval, direction, trade_state, \
          actual_entry_time, actual_entry_price, primary_target, reference_levels, \
          initial_stop, active_stop, release_id, algorithm_version, parameter_version, \
          execution_model, ambiguity) \
         VALUES ($1, $2, 'XAUUSDT', 'ICT', '1m', 'BUY', 'OPEN', $3, 4100, 4120, \
                 '[]', 4090, 4090, $4, 'ict-v1', 'ict-1m-v1', 'LIVE_TRADE', 'NONE')",
    )
    .bind(virtual_trade_id)
    .bind(signal_id)
    .bind(NOW_MS + 1)
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    let restored = repository.load_active(&symbol()).await.unwrap();
    let legacy = &restored.active_trades[0];
    assert_eq!(legacy.position_stage, PositionStage::Legacy);
    assert_eq!(legacy.trade_health, TradeHealth::Healthy);
    assert_eq!(legacy.remaining_fraction, Decimal::ONE);
    assert_eq!(legacy.realized_fraction, Decimal::ZERO);
    assert_eq!(legacy.realized_r, Decimal::ZERO);
    assert_eq!(legacy.tp1_price, None);
    assert_eq!(legacy.tp2_zone_low, None);
    assert_eq!(legacy.tp2_zone_high, None);

    let snapshot = MonitoringRepository::new(pool.clone())
        .snapshot()
        .await
        .unwrap();
    let monitored = &snapshot.virtual_trades[0];
    let expected_payload = virtual_trade_payload(legacy);
    assert_lifecycle_payload_parity(&expected_payload, monitored, "legacy snapshot");
    assert_eq!(monitored["positionStage"], "LEGACY");
    assert_eq!(monitored["tradeHealth"], "HEALTHY");
    assert_eq!(monitored["remainingFraction"], "1");
    assert_eq!(monitored["realizedFraction"], "0");
    assert_eq!(monitored["realizedR"], "0");
    assert!(monitored["thesisInvalidation"].is_null());
    assert!(monitored["hardStop"].is_null());
    assert!(monitored["tp1Price"].is_null());
    assert!(monitored["tp1Fraction"].is_null());
    assert_eq!(monitored["tp1Filled"], false);
    assert!(monitored["tp1FilledAt"].is_null());
    assert!(monitored["tp2Zone"].is_null());
    assert!(monitored["tp2ExecutionEdge"].is_null());
    assert_eq!(monitored["analyticalTarget"], "4120");
    assert_eq!(monitored["ambiguity"], "NONE");
    assert!(monitored["reconciliationSource"].is_null());

    let history = VirtualTradeRepository::new(pool.clone());
    let page = history
        .history(VirtualTradeHistoryQuery {
            symbol: Some("XAUUSDT".to_owned()),
            plan: None,
            timeframe: None,
            direction: None,
            exit_reason: None,
            from: None,
            to: None,
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert_lifecycle_payload_parity(&expected_payload, &page.items[0], "legacy history");
    let detail = history
        .detail(virtual_trade_id)
        .await
        .unwrap()
        .expect("legacy detail");
    assert_lifecycle_payload_parity(&expected_payload, &detail.trade, "legacy detail");
    pool.close().await;
}

#[tokio::test]
async fn monitoring_snapshot_rejects_malformed_referenced_signal() {
    let (pool, _guard) = test_pool().await;
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO signals \
         (signal_id, symbol, plan, interval, definition, evidence, release_id, \
          algorithm_version, parameter_version, created_at) \
         VALUES ($1, 'XAUUSDT', 'ICT', '1m', '{}'::jsonb, '[]', $2, \
                 'ict-v1', 'ict-1m-v1', NOW())",
    )
    .bind(signal_id)
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();
    TrackerRepository::new(pool.clone())
        .commit_transition(transition(signal_id, release_id))
        .await
        .unwrap();

    let error = MonitoringRepository::new(pool.clone())
        .snapshot()
        .await
        .expect_err("malformed referenced signal must fail the snapshot");

    assert!(matches!(
        error,
        execution_service::ExecutionError::InvalidData(_)
    ));
    assert!(error.to_string().contains("signal"));
    pool.close().await;
}

#[tokio::test]
async fn market_cursor_checkpoint_survives_reload_and_never_regresses() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    repository
        .checkpoint_cursor(MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 200,
            last_trade_time: NOW_MS + 20,
        })
        .await
        .unwrap();
    repository
        .checkpoint_cursor(MarketCursor {
            symbol: symbol(),
            last_processed_agg_trade_id: 150,
            last_trade_time: NOW_MS + 30,
        })
        .await
        .unwrap();

    let cursor = repository
        .load_active(&symbol())
        .await
        .unwrap()
        .cursor
        .unwrap();
    assert_eq!(cursor.last_processed_agg_trade_id, 200);
    assert_eq!(cursor.last_trade_time, NOW_MS + 20);
    pool.close().await;
}

#[tokio::test]
async fn ohlc_reconciliation_metadata_survives_trade_update() {
    let (pool, _guard) = test_pool().await;
    let repository = TrackerRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;
    repository
        .commit_transition(transition(signal_id, release_id))
        .await
        .unwrap();

    let mut closed = repository
        .load_active(&symbol())
        .await
        .unwrap()
        .active_trades[0]
        .clone();
    closed.trade_state = TradeState::Closed;
    closed.exit_time = Some(NOW_MS + 60_000);
    closed.exit_price = Some(closed.active_stop);
    closed.execution_model = ExecutionModel::OhlcConservative;
    closed.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
    closed.ambiguity = Ambiguity::Ambiguous;
    let virtual_trade_id = closed.virtual_trade_id;
    repository
        .commit_transition(DurableTransition {
            virtual_trade_write: Some(VirtualTradeWrite { trade: closed }),
            monitoring_events: vec![NewMonitoringEvent {
                event_id: Uuid::now_v7(),
                event_type: "VIRTUAL_TRADE_CLOSED".to_owned(),
                entity_type: "VIRTUAL_TRADE".to_owned(),
                entity_id: Some(virtual_trade_id),
                payload: json!({"virtualTradeId": virtual_trade_id}),
            }],
            ..DurableTransition::default()
        })
        .await
        .unwrap();

    let row: (String, Option<String>, String) = sqlx::query_as(
        "SELECT execution_model, reconciliation_source, ambiguity \
         FROM virtual_trades WHERE signal_id = $1",
    )
    .bind(signal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "OHLC_CONSERVATIVE");
    assert_eq!(row.1.as_deref(), Some("OHLC_RECONCILED"));
    assert_eq!(row.2, "AMBIGUOUS");
    pool.close().await;
}

#[tokio::test]
async fn scalping_audit_repository_persists_snapshots_for_later_review() {
    let (pool, _guard) = test_pool().await;
    let repository = ScalpingAuditRepository::new(pool.clone());

    let first = repository
        .create(NewScalpingAuditSnapshot {
            symbol: "XAUUSDT".to_owned(),
            mode: "SCALPING".to_owned(),
            timeframes: vec!["1m".to_owned(), "5m".to_owned(), "15m".to_owned()],
            report: json!({"lockedProfiles": 18, "totalProfiles": 18}),
            journal: json!({"latestCursor": 99, "events": []}),
            audit: json!({"rows": [{"timeframe": "1m", "plan": "ICT", "status": "LOW_PERFORMER"}]}),
        })
        .await
        .unwrap();
    let second = repository
        .create(NewScalpingAuditSnapshot {
            symbol: "XAUUSDT".to_owned(),
            mode: "SCALPING".to_owned(),
            timeframes: vec!["1m".to_owned(), "5m".to_owned(), "15m".to_owned()],
            report: json!({"lockedProfiles": 18, "totalProfiles": 18}),
            journal: json!({"latestCursor": 100, "events": []}),
            audit: json!({"rows": [{"timeframe": "1m", "plan": "ICT", "status": "OK"}]}),
        })
        .await
        .unwrap();

    let page = repository
        .list(execution_service::ScalpingAuditSnapshotQuery {
            symbol: Some("XAUUSDT".to_owned()),
            cursor: None,
            limit: 1,
        })
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].snapshot_id, second.snapshot_id);
    assert_eq!(page.items[0].timeframes, ["1m", "5m", "15m"]);
    assert_eq!(page.items[0].report["lockedProfiles"], 18);
    assert_eq!(page.items[0].audit["rows"][0]["status"], "OK");
    assert!(page.next_cursor.is_some());

    let last_page = repository
        .list(execution_service::ScalpingAuditSnapshotQuery {
            symbol: Some("XAUUSDT".to_owned()),
            cursor: page.next_cursor,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(last_page.items.len(), 1);
    assert_eq!(last_page.items[0].snapshot_id, first.snapshot_id);
    assert_eq!(last_page.next_cursor, None);
    pool.close().await;
}

#[tokio::test]
async fn scalping_audit_pages_do_not_skip_snapshots_sharing_a_timestamp() {
    let (pool, _guard) = test_pool().await;
    let repository = ScalpingAuditRepository::new(pool.clone());
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO scalping_audit_snapshots \
             (snapshot_id, symbol, mode, timeframes, report, journal, audit, created_at) \
             VALUES ($1, 'XAUUSDT', 'SCALPING', ARRAY['1m'], '{}', '{}', '{}', \
                     '2026-10-31T00:00:00.123456Z')",
        )
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = repository
            .list(execution_service::ScalpingAuditSnapshotQuery {
                symbol: Some("XAUUSDT".to_owned()),
                cursor,
                limit: 1,
            })
            .await
            .unwrap();
        seen.extend(page.items.iter().map(|snapshot| snapshot.snapshot_id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3);
    pool.close().await;
}

#[tokio::test]
async fn monitoring_snapshot_includes_active_signal_recommendations_for_overlay() {
    let (pool, _guard) = test_pool().await;
    let repository = MonitoringRepository::new(pool.clone());
    let signal_id = Uuid::now_v7();
    let release_id = Uuid::now_v7();
    let mut definition = signal(signal_id, release_id);
    definition.stop_loss = Decimal::new(4_085, 0);
    definition.primary_target = Decimal::new(4_130, 0);
    definition.execution_plan = Some(SignalExecutionPlan {
        thesis_invalidation: Decimal::new(4_092, 0),
        hard_stop: Decimal::new(4_085, 0),
        tp1: TargetLeg {
            trigger_r: Decimal::ONE,
            fraction: Decimal::new(5, 1),
        },
        tp2_zone: PriceZone::new(Decimal::new(4_128, 0), Decimal::new(4_130, 0)).unwrap(),
    });
    let expected = signal_recommendation_payload(&definition);
    seed_signal_definition(&pool, definition).await;
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
    )
    .bind(signal_id)
    .bind(json!({
        "signalId": signal_id,
        "setupState": "ENTRY_AVAILABLE",
        "tradeState": "NOT_ENTERED"
    }))
    .execute(&pool)
    .await
    .unwrap();

    let snapshot = repository.snapshot().await.unwrap();

    assert_eq!(snapshot.recommendations.len(), 1);
    let recommendation = &snapshot.recommendations[0];
    assert_eq!(recommendation["signalId"], signal_id.to_string());
    assert_eq!(recommendation["plan"], "ICT");
    assert_eq!(recommendation["timeframe"], "1m");
    assert_eq!(recommendation["entryLow"], "4100");
    assert_eq!(recommendation["entryHigh"], "4101");
    for key in [
        "analyticalTarget",
        "thesisInvalidation",
        "hardStop",
        "tp1Price",
        "tp1R",
        "tp1Fraction",
        "tp1Filled",
        "tp1FilledAt",
        "tp2Zone",
        "tp2ExecutionEdge",
    ] {
        assert_eq!(
            recommendation.get(key),
            expected.get(key),
            "recommendation.{key}"
        );
    }
    assert_eq!(recommendation["stopLoss"], "4085");
    assert_eq!(recommendation["primaryTarget"], "4130");
    pool.close().await;
}

#[tokio::test]
async fn monitoring_snapshot_includes_active_plan_runtime_summaries() {
    let (pool, _guard) = test_pool().await;
    let repository = MonitoringRepository::new(pool.clone());
    let release_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO configuration_releases \
         (release_id, symbol, mode, lifecycle, validation_summary, activated_at) \
         VALUES ($1, 'XAUUSDT', 'PAPER_ACTIVE', 'ACTIVE', '{}', NOW())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO plan_runtime_states \
         (symbol, plan, interval, release_id, state, last_evaluated_close_time) \
         VALUES ('XAUUSDT', 'RSI', '5m', $1, $2, $3)",
    )
    .bind(release_id)
    .bind(json!({
        "readiness": {"state": "READY"},
        "setupState": "WATCHING",
        "direction": "BUY",
        "tradeState": "NOT_ENTERED",
        "activeSignal": null,
        "activeVirtualTradeId": null
    }))
    .bind(NOW_MS)
    .execute(&pool)
    .await
    .unwrap();

    let snapshot = repository.snapshot().await.unwrap();

    assert_eq!(snapshot.summaries.len(), 1);
    let summary = &snapshot.summaries[0];
    assert_eq!(summary["symbol"], "XAUUSDT");
    assert_eq!(summary["plan"], "RSI");
    assert_eq!(summary["timeframe"], "5m");
    assert_eq!(summary["readiness"], "READY");
    assert_eq!(summary["setupState"], "WATCHING");
    assert_eq!(summary["tradeState"], "NOT_ENTERED");
    assert_eq!(summary["direction"], "BUY");
    assert_eq!(summary["trackingHealth"], "LIVE");
    assert_eq!(summary["evaluatedClosedCandleTime"], NOW_MS);
    pool.close().await;
}

#[tokio::test]
async fn monitoring_snapshot_hides_stale_plan_summary_active_signal() {
    let (pool, _guard) = test_pool().await;
    let repository = MonitoringRepository::new(pool.clone());
    let release_id = Uuid::now_v7();
    let signal_id = Uuid::now_v7();
    seed_signal(&pool, signal_id, release_id).await;
    sqlx::query(
        "INSERT INTO configuration_releases \
         (release_id, symbol, mode, lifecycle, validation_summary, activated_at) \
         VALUES ($1, 'XAUUSDT', 'PAPER_ACTIVE', 'ACTIVE', '{}', NOW())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
    )
    .bind(signal_id)
    .bind(json!({
        "signalId": signal_id,
        "setupState": "INVALIDATED",
        "tradeState": "NOT_ENTERED"
    }))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO plan_runtime_states \
         (symbol, plan, interval, release_id, state, last_evaluated_close_time) \
         VALUES ('XAUUSDT', 'ICT', '1m', $1, $2, $3)",
    )
    .bind(release_id)
    .bind(json!({
        "readiness": {"state": "READY"},
        "setupState": "ENTRY_AVAILABLE",
        "direction": "SELL",
        "tradeState": "NOT_ENTERED",
        "activeSignal": {"signalId": signal_id},
        "activeVirtualTradeId": null
    }))
    .bind(NOW_MS)
    .execute(&pool)
    .await
    .unwrap();

    let snapshot = repository.snapshot().await.unwrap();

    assert!(snapshot.recommendations.is_empty());
    let summary = &snapshot.summaries[0];
    assert_eq!(summary["setupState"], "WATCHING");
    assert_eq!(summary["tradeState"], "NOT_ENTERED");
    assert_eq!(summary["direction"], "NONE");
    assert_eq!(summary["activeSignalId"], serde_json::Value::Null);
    assert_eq!(summary["activeVirtualTradeId"], serde_json::Value::Null);
    pool.close().await;
}

#[tokio::test]
async fn monitoring_event_cursor_order_matches_commit_order() {
    let (pool, _guard) = test_pool().await;
    let insert = "INSERT INTO monitoring_events \
                  (event_id, event_type, entity_type, entity_id, payload) \
                  VALUES ($1, 'TEST', 'test', NULL, '{}'::jsonb) RETURNING cursor";
    let first_id = Uuid::now_v7();
    let mut first = pool.begin().await.unwrap();
    let first_cursor: i64 = sqlx::query_scalar(insert)
        .bind(first_id)
        .fetch_one(&mut *first)
        .await
        .unwrap();

    let second_pool = pool.clone();
    let second = tokio::spawn(async move {
        let mut transaction = second_pool.begin().await.unwrap();
        let cursor: i64 = sqlx::query_scalar(insert)
            .bind(Uuid::now_v7())
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        cursor
    });
    // The second writer must queue behind the uncommitted first one.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second writer waits for the cursor lock");
    assert!(!second.is_finished());

    first.commit().await.unwrap();
    let second_cursor = second.await.unwrap();
    assert!(first_cursor < second_cursor);
}
