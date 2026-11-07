use analysis_domain::{
    Direction, ExecutionInstruction, HtfAlignment, HtfContext, PlanKind, PreEntryValidation,
    PriceZone, SignalDefinition, SignalExecutionPlan, TargetLeg, VersionIdentity,
};
use execution_domain::TrackingHealth;
use execution_domain::{
    Ambiguity, ExecutionModel, PositionStage, ReconciliationSource, TradeHealth, TradeState,
    VirtualTrade,
};
use execution_service::{
    MonitoringEventEnvelope, MonitoringSnapshot, OutboxDelivery, OutboxRetention,
    OutboxStreamBatch, signal_recommendation_payload, virtual_trade_payload,
    virtual_trade_payload_with_signal,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

#[test]
fn snapshot_contract_is_complete_and_uses_latest_cursor() {
    let snapshot = MonitoringSnapshot {
        recommendations: vec![json!({"signalId": Uuid::now_v7(), "entry": "4100.00"})],
        virtual_trades: vec![
            json!({"virtualTradeId": Uuid::now_v7(), "actualEntryPrice": "4105.00"}),
        ],
        summaries: vec![json!({"plan": "ICT", "state": "OPEN"})],
        health: BTreeMap::from([("XAUUSDT".to_owned(), TrackingHealth::Live)]),
        latest_cursor: 7,
    };

    assert_eq!(snapshot.latest_cursor, 7);
    assert_eq!(snapshot.health["XAUUSDT"], TrackingHealth::Live);
    assert!(snapshot.virtual_trades[0]["actualEntryPrice"].is_string());
}

#[test]
fn recommendation_payload_includes_htf_context() {
    let signal = signal_with_htf_context();
    let payload = signal_recommendation_payload(&signal);

    assert_eq!(payload["htfContext"]["higherTimeframe"], "5m");
    assert_eq!(payload["htfContext"]["direction"], "SELL");
    assert_eq!(payload["htfContext"]["alignment"], "COUNTER");
    assert_eq!(payload["htfContext"]["source"], "STRUCTURE");
}

#[test]
fn recommendation_payload_projects_execution_plan_without_frontend_math() {
    for (direction, analytical_target, thesis, hard_stop, zone_low, zone_high, edge) in [
        (
            Direction::Buy,
            Decimal::new(4_140, 0),
            Decimal::new(4_090, 0),
            Decimal::new(4_080, 0),
            Decimal::new(4_138, 0),
            Decimal::new(4_140, 0),
            "4138",
        ),
        (
            Direction::Sell,
            Decimal::new(4_060, 0),
            Decimal::new(4_110, 0),
            Decimal::new(4_120, 0),
            Decimal::new(4_060, 0),
            Decimal::new(4_062, 0),
            "4062",
        ),
    ] {
        let mut signal = signal_with_htf_context();
        signal.direction = direction;
        signal.primary_target = analytical_target;
        signal.stop_loss = hard_stop;
        signal.execution_plan = Some(SignalExecutionPlan {
            thesis_invalidation: thesis,
            hard_stop,
            tp1: TargetLeg {
                trigger_r: Decimal::ONE,
                fraction: Decimal::new(5, 1),
            },
            tp2_zone: PriceZone::new(zone_low, zone_high).unwrap(),
        });

        let payload = signal_recommendation_payload(&signal);

        assert_eq!(payload["analyticalTarget"], analytical_target.to_string());
        assert_eq!(payload["thesisInvalidation"], thesis.to_string());
        assert_eq!(payload["hardStop"], hard_stop.to_string());
        assert!(payload["tp1Price"].is_null());
        assert_eq!(payload["tp1Fraction"], "0.5");
        assert_eq!(payload["tp1Filled"], false);
        assert!(payload["tp1FilledAt"].is_null());
        assert_eq!(payload["tp2Zone"]["low"], zone_low.to_string());
        assert_eq!(payload["tp2Zone"]["high"], zone_high.to_string());
        assert_eq!(payload["tp2ExecutionEdge"], edge);
    }
}

#[test]
fn legacy_recommendation_payload_keeps_execution_plan_fields_explicit() {
    let payload = signal_recommendation_payload(&signal_with_htf_context());

    assert_eq!(payload["analyticalTarget"], "4140");
    assert!(payload["thesisInvalidation"].is_null());
    assert!(payload["hardStop"].is_null());
    assert!(payload["tp1Price"].is_null());
    assert!(payload["tp1Fraction"].is_null());
    assert_eq!(payload["tp1Filled"], false);
    assert!(payload["tp1FilledAt"].is_null());
    assert!(payload["tp2Zone"].is_null());
    assert!(payload["tp2ExecutionEdge"].is_null());
}

#[test]
fn virtual_trade_payload_inherits_signal_htf_context() {
    let signal = signal_with_htf_context();
    let trade = VirtualTrade {
        virtual_trade_id: Uuid::now_v7(),
        signal_id: signal.signal_id,
        symbol: signal.symbol.clone(),
        plan: signal.plan,
        timeframe: signal.target_timeframe,
        direction: signal.direction,
        trade_state: TradeState::Open,
        position_stage: PositionStage::Legacy,
        trade_health: TradeHealth::Healthy,
        actual_entry_time: signal.effective_at,
        actual_entry_price: Decimal::new(4_105, 0),
        exit_time: None,
        exit_price: None,
        primary_target: signal.primary_target,
        reference_levels: Vec::new(),
        initial_stop: signal.stop_loss,
        active_stop: signal.stop_loss,
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
            release_id: signal.release_id,
            algorithm_version: signal.algorithm_version.clone(),
            parameter_version: signal.parameter_version.clone(),
        },
        execution_model: ExecutionModel::LiveTrade,
        reconciliation_source: Some(ReconciliationSource::ExactTrade),
        ambiguity: Ambiguity::None,
        gross_r: None,
        mfe_r: Decimal::ZERO,
        mae_r: Decimal::ZERO,
        duration_ms: None,
        exit_reason: None,
    };

    let payload = virtual_trade_payload_with_signal(&trade, Some(&signal));

    assert_eq!(payload["htfContext"]["higherTimeframe"], "5m");
    assert_eq!(payload["htfContext"]["alignment"], "COUNTER");
    assert_eq!(payload["positionStage"], "LEGACY");
    assert_eq!(payload["remainingFraction"], "1");
    assert_eq!(payload["realizedFraction"], "0");
    assert_eq!(payload["realizedR"], "0");
    assert_eq!(payload["tradeHealth"], "HEALTHY");
    assert!(payload["hardStop"].is_null());
    assert_eq!(payload["activeStop"], "4080");
    assert_eq!(payload["tp1Filled"], false);
    assert!(payload["tp2Zone"].is_null());
    assert!(payload["tp2ExecutionEdge"].is_null());
    assert_eq!(payload["analyticalTarget"], "4140");
    assert_eq!(payload["ambiguity"], "NONE");
    assert_eq!(payload["reconciliationSource"], "EXACT_TRADE");

    let mut runner = trade;
    runner.position_stage = PositionStage::Runner;
    runner.trade_health = TradeHealth::Weakening;
    runner.active_stop = Decimal::new(4_105, 0);
    runner.thesis_invalidation = Some(Decimal::new(4_090, 0));
    runner.tp1_price = Some(Decimal::new(4_110, 0));
    runner.tp1_r = Some(Decimal::ONE);
    runner.tp1_fraction = Some(Decimal::new(5, 1));
    runner.tp1_filled_at = Some(1_782_768_720_000);
    runner.remaining_fraction = Decimal::new(5, 1);
    runner.realized_fraction = Decimal::new(5, 1);
    runner.realized_r = Decimal::new(5, 1);
    runner.tp2_zone_low = Some(Decimal::new(4_138, 0));
    runner.tp2_zone_high = Some(Decimal::new(4_140, 0));
    runner.execution_model = ExecutionModel::OhlcConservative;
    runner.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
    runner.ambiguity = Ambiguity::Ambiguous;

    let payload = virtual_trade_payload(&runner);

    assert_eq!(payload["positionStage"], "RUNNER");
    assert_eq!(payload["remainingFraction"], "0.5");
    assert_eq!(payload["realizedFraction"], "0.5");
    assert_eq!(payload["realizedR"], "0.5");
    assert_eq!(payload["tradeHealth"], "WEAKENING");
    assert_eq!(payload["thesisInvalidation"], "4090");
    assert_eq!(payload["hardStop"], "4080");
    assert_eq!(payload["activeStop"], "4105");
    assert_eq!(payload["tp1Price"], "4110");
    assert_eq!(payload["tp1R"], "1");
    assert_eq!(payload["tp1Fraction"], "0.5");
    assert_eq!(payload["tp1Filled"], true);
    assert_eq!(payload["tp1FilledAt"], 1_782_768_720_000_i64);
    assert_eq!(payload["tp2Zone"]["low"], "4138");
    assert_eq!(payload["tp2Zone"]["high"], "4140");
    assert_eq!(payload["tp2ExecutionEdge"], "4138");
    assert_eq!(payload["analyticalTarget"], "4140");
    assert_eq!(payload["ambiguity"], "AMBIGUOUS");
    assert_eq!(payload["reconciliationSource"], "OHLC_RECONCILED");
}

#[test]
fn cursor_reconnect_orders_events_and_allows_at_least_once_duplicates() {
    let batch = OutboxStreamBatch::from_events(
        1,
        vec![
            event(1, "trade-triggered"),
            event(2, "trade-closed"),
            event(2, "trade-closed"),
        ],
    );

    assert!(!batch.resync_required);
    assert_eq!(
        batch
            .events
            .iter()
            .map(|event| event.cursor)
            .collect::<Vec<_>>(),
        vec![1, 2, 2]
    );
}

#[test]
fn old_cursor_requires_resync_when_before_oldest_retained_cursor() {
    let batch = OutboxStreamBatch::resync_required(9);

    assert!(batch.resync_required);
    assert_eq!(batch.events[0].event_type, "resync-required");
    assert_eq!(batch.events[0].cursor, 9);
}

#[test]
fn publish_after_commit_marks_only_offered_events() {
    let mut delivery = OutboxDelivery::new(vec![event(1, "trade-triggered"), event(2, "health")]);

    delivery.mark_offered(1);

    assert_eq!(delivery.published_cursors(), vec![1]);
    assert_eq!(delivery.pending_cursors(), vec![2]);
}

#[test]
fn retention_deletes_only_published_old_rows_and_preserves_pending_or_failed() {
    let retention = OutboxRetention::new(30, 500);
    let rows = vec![
        retention.row(1, 0, Some(1), 0, None),
        retention.row(2, 0, None, 0, None),
        retention.row(3, 0, Some(1), 1, Some("network".to_owned())),
        retention.row(4, retention.cutoff_ms() + 1, Some(1), 0, None),
    ];

    assert_eq!(retention.deletable_cursors(&rows), vec![1]);
}

fn event(cursor: i64, event_type: &str) -> MonitoringEventEnvelope {
    MonitoringEventEnvelope {
        cursor,
        event_id: Uuid::now_v7(),
        event_type: event_type.to_owned(),
        entity_type: "virtual_trade".to_owned(),
        entity_id: None,
        payload: json!({"cursor": cursor}),
    }
}

fn signal_with_htf_context() -> SignalDefinition {
    SignalDefinition {
        signal_id: Uuid::now_v7(),
        symbol: Symbol::new("XAUUSDT").unwrap(),
        plan: PlanKind::Rsi,
        target_timeframe: Timeframe::M1,
        evaluated_at: 1_782_768_600_000,
        direction: Direction::Buy,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(Decimal::new(4_100, 0)),
        stop_loss: Decimal::new(4_080, 0),
        primary_target: Decimal::new(4_140, 0),
        execution_plan: None,
        reference_levels: Vec::new(),
        risk_reward: Decimal::new(2, 0),
        pre_entry_validation: Some(PreEntryValidation {
            minimum_rr: Decimal::ONE,
            reject_if_target_crossed: true,
            reject_if_stop_crossed: true,
        }),
        confidence: Decimal::ONE,
        evidence: Vec::new(),
        htf_context: Some(HtfContext {
            higher_timeframe: Some(Timeframe::M5),
            direction: Direction::Sell,
            alignment: HtfAlignment::Counter,
            source: "STRUCTURE".to_owned(),
        }),
        effective_at: 1_782_768_660_000,
        detected_at: 1_782_768_600_000,
        published_at: 1_782_768_600_000,
        release_id: Uuid::now_v7(),
        algorithm_version: "rsi-v1".to_owned(),
        parameter_version: "rsi-1m-test".to_owned(),
        created_at: 1_782_768_600_000,
        expires_at: Some(1_782_769_200_000),
    }
}
