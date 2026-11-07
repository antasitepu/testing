use analysis_domain::{
    AnalysisWorkItem, DirectiveKind, ManagementDirective, PlanProfileKey, StructuredAnalysisError,
    WorkSource, WorkStatus,
};
use analysis_service::{
    AnalysisServiceError, AnalysisWorkStore, EvaluationCommit,
    MonitoringEvent as AnalysisMonitoringEvent, PlanRuntimeStateRecord,
};
use async_trait::async_trait;
use execution_domain::{MarketCursor, MonitoringEvent as DomainMonitoringEvent, TrackingHealth};
use execution_service::{
    DurableTransition, ExecutionError, HealthTransition, NewMonitoringEvent, TrackerSnapshot,
    TrackerState, TrackerStore,
};
use gold_monitoring_server::execution_wake::{
    ExecutionBarrier, ExecutionWakeSource, NotifyingAnalysisWorkStore, NotifyingTrackerStore,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;
use uuid::Uuid;

#[tokio::test]
async fn leased_work_waits_until_execution_reaches_its_close_time() {
    let inner = Arc::new(FakeWorkStore::default());
    *inner.next_work.lock().unwrap() = Some(work_item(59_999));
    let (barrier, mut requests) = ExecutionBarrier::channel();
    let store = NotifyingAnalysisWorkStore::new(inner, ExecutionWakeSource::new())
        .with_execution_barrier(barrier);

    let lease = tokio::spawn(async move { store.lease_next("worker", 0).await });
    let request = timeout(Duration::from_secs(1), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.close_time, 59_999);
    assert!(!lease.is_finished());

    request.done.send(true).unwrap();
    let work = timeout(Duration::from_secs(1), lease)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(work.unwrap().canonical_close_time, 59_999);
}

#[tokio::test]
async fn work_is_deferred_while_execution_cannot_reach_its_close_time() {
    let inner = Arc::new(FakeWorkStore::default());
    *inner.next_work.lock().unwrap() = Some(work_item(59_999));
    let (barrier, mut requests) = ExecutionBarrier::channel();
    let store = NotifyingAnalysisWorkStore::new(inner.clone(), ExecutionWakeSource::new())
        .with_execution_barrier(barrier);

    let lease = tokio::spawn(async move { store.lease_next("worker", 0).await });
    let request = timeout(Duration::from_secs(1), requests.recv())
        .await
        .unwrap()
        .unwrap();
    request.done.send(false).unwrap();

    let work = timeout(Duration::from_secs(1), lease)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(work.is_none());
    assert_eq!(inner.deferred.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn idle_lease_does_not_ask_execution_to_advance() {
    let (barrier, mut requests) = ExecutionBarrier::channel();
    let store = NotifyingAnalysisWorkStore::new(
        Arc::new(FakeWorkStore::default()),
        ExecutionWakeSource::new(),
    )
    .with_execution_barrier(barrier);

    assert!(store.lease_next("worker", 0).await.unwrap().is_none());
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn leasing_fails_once_execution_has_stopped() {
    let inner = Arc::new(FakeWorkStore::default());
    *inner.next_work.lock().unwrap() = Some(work_item(59_999));
    let (barrier, requests) = ExecutionBarrier::channel();
    drop(requests);
    let store = NotifyingAnalysisWorkStore::new(inner, ExecutionWakeSource::new())
        .with_execution_barrier(barrier);

    assert!(store.lease_next("worker", 0).await.is_err());
}

#[tokio::test]
async fn notifying_work_store_wakes_execution_after_signal_or_directive_commit() {
    let wake = ExecutionWakeSource::new();
    let mut receiver = wake.subscribe();
    let inner = Arc::new(FakeWorkStore::default());
    let store = NotifyingAnalysisWorkStore::new(inner.clone(), wake);

    store
        .complete_evaluation(EvaluationCommit {
            work_id: Uuid::now_v7(),
            directives: vec![directive()],
            ..EvaluationCommit::default()
        })
        .await
        .unwrap();

    timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("execution wake should be delivered")
        .unwrap();
    assert_eq!(*receiver.borrow(), 1);
    assert_eq!(inner.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn notifying_work_store_wakes_monitoring_after_monitoring_event_commit() {
    let execution_wake = ExecutionWakeSource::new();
    let monitoring_wake = ExecutionWakeSource::new();
    let mut receiver = monitoring_wake.subscribe();
    let inner = Arc::new(FakeWorkStore::default());
    let store = NotifyingAnalysisWorkStore::new(inner.clone(), execution_wake)
        .with_monitoring_wake(monitoring_wake);

    store
        .complete_evaluation(EvaluationCommit {
            work_id: Uuid::now_v7(),
            monitoring_events: vec![analysis_monitoring_event("SIGNAL_RECOMMENDATION")],
            ..EvaluationCommit::default()
        })
        .await
        .unwrap();

    timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("monitoring wake should be delivered after outbox commit")
        .unwrap();
    assert_eq!(*receiver.borrow(), 1);
    assert_eq!(inner.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn notifying_work_store_does_not_wake_on_empty_or_failed_commit() {
    let wake = ExecutionWakeSource::new();
    let mut receiver = wake.subscribe();
    let inner = Arc::new(FakeWorkStore::default());
    let store = NotifyingAnalysisWorkStore::new(inner.clone(), wake);

    store
        .complete_evaluation(EvaluationCommit {
            work_id: Uuid::now_v7(),
            ..EvaluationCommit::default()
        })
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(20), receiver.changed())
            .await
            .is_err()
    );

    inner.fail_next.store(true, Ordering::SeqCst);
    let result = store
        .complete_evaluation(EvaluationCommit {
            work_id: Uuid::now_v7(),
            directives: vec![directive()],
            ..EvaluationCommit::default()
        })
        .await;

    assert!(result.is_err());
    assert!(
        timeout(Duration::from_millis(20), receiver.changed())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn notifying_tracker_store_wakes_monitoring_after_durable_transition_or_health_commit() {
    let wake = ExecutionWakeSource::new();
    let mut receiver = wake.subscribe();
    let inner = Arc::new(FakeTrackerStore::default());
    let store = NotifyingTrackerStore::new(inner.clone(), wake);

    store
        .commit_transition(DurableTransition {
            monitoring_events: vec![new_monitoring_event("VIRTUAL_TRADE_OPENED")],
            ..DurableTransition::default()
        })
        .await
        .unwrap();
    timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("monitoring wake should be delivered after tracker transition")
        .unwrap();
    assert_eq!(*receiver.borrow(), 1);

    store
        .mark_health(HealthTransition {
            state: tracker_state(),
            monitoring_event: new_monitoring_event("TRACKER_HEALTH_CHANGED"),
        })
        .await
        .unwrap();
    timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("monitoring wake should be delivered after health transition")
        .unwrap();
    assert_eq!(*receiver.borrow(), 2);
    assert_eq!(inner.transitions.load(Ordering::SeqCst), 1);
    assert_eq!(inner.health_updates.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct FakeWorkStore {
    commits: AtomicUsize,
    fail_next: AtomicBool,
    next_work: Mutex<Option<AnalysisWorkItem>>,
    deferred: AtomicUsize,
}

fn work_item(canonical_close_time: i64) -> AnalysisWorkItem {
    AnalysisWorkItem {
        work_id: Uuid::now_v7(),
        symbol: Symbol::new("XAUUSDT").unwrap(),
        timeframe: Timeframe::M1,
        canonical_close_time,
        release_id: Uuid::now_v7(),
        source: WorkSource::CatchUp,
        status: WorkStatus::Pending,
        lease_owner: None,
        lease_expires_at: None,
        attempt_count: 0,
        error: None,
        created_at: 0,
        completed_at: None,
    }
}

#[async_trait]
impl AnalysisWorkStore for FakeWorkStore {
    async fn lease_next(
        &self,
        _owner: &str,
        _now_ms: i64,
    ) -> Result<Option<AnalysisWorkItem>, AnalysisServiceError> {
        Ok(self.next_work.lock().unwrap().take())
    }

    async fn complete_evaluation(
        &self,
        _commit: EvaluationCommit,
    ) -> Result<(), AnalysisServiceError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(AnalysisServiceError::Storage {
                operation: "fake complete evaluation",
                message: "injected failure".to_owned(),
            });
        }
        Ok(())
    }

    async fn runtime_state(
        &self,
        _key: &PlanProfileKey,
        _release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError> {
        Ok(None)
    }

    async fn defer(&self, _work_id: Uuid, _lease_attempt: u32) -> Result<(), AnalysisServiceError> {
        self.deferred.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn fail(
        &self,
        _work_id: Uuid,
        _lease_attempt: u32,
        _error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeTrackerStore {
    transitions: AtomicUsize,
    health_updates: AtomicUsize,
}

#[async_trait]
impl TrackerStore for FakeTrackerStore {
    async fn load_active(&self, _symbol: &Symbol) -> Result<TrackerSnapshot, ExecutionError> {
        Ok(TrackerSnapshot::default())
    }

    async fn checkpoint_cursor(&self, _cursor: MarketCursor) -> Result<(), ExecutionError> {
        Ok(())
    }

    async fn commit_transition(
        &self,
        _transition: DurableTransition,
    ) -> Result<Option<DomainMonitoringEvent>, ExecutionError> {
        self.transitions.fetch_add(1, Ordering::SeqCst);
        Ok(Some(domain_monitoring_event("virtual-trade-opened")))
    }

    async fn mark_health(
        &self,
        _change: HealthTransition,
    ) -> Result<DomainMonitoringEvent, ExecutionError> {
        self.health_updates.fetch_add(1, Ordering::SeqCst);
        Ok(domain_monitoring_event("tracker-health-changed"))
    }
}

fn directive() -> ManagementDirective {
    ManagementDirective {
        directive_id: Uuid::now_v7(),
        signal_id: Uuid::now_v7(),
        virtual_trade_id: None,
        kind: DirectiveKind::MoveStop,
        effective_at: 1_782_195_600_000,
        price: Some(Decimal::new(4100, 0)),
        reason: "test directive".to_owned(),
        source_closed_candle_time: 1_782_195_600_000,
        algorithm_version: "test-v1".to_owned(),
        parameter_version: "test-params-v1".to_owned(),
        reverse_signal: None,
    }
}

fn new_monitoring_event(event_type: &str) -> NewMonitoringEvent {
    NewMonitoringEvent {
        event_id: Uuid::now_v7(),
        event_type: event_type.to_owned(),
        entity_type: "signal".to_owned(),
        entity_id: Some(Uuid::now_v7()),
        payload: json!({"symbol": "XAUUSDT"}),
    }
}

fn analysis_monitoring_event(event_type: &str) -> AnalysisMonitoringEvent {
    AnalysisMonitoringEvent {
        event_id: Uuid::now_v7(),
        event_type: event_type.to_owned(),
        entity_type: "signal".to_owned(),
        entity_id: Some(Uuid::now_v7()),
        payload: json!({"symbol": "XAUUSDT"}),
    }
}

fn domain_monitoring_event(event_type: &str) -> DomainMonitoringEvent {
    DomainMonitoringEvent {
        cursor: 1,
        event_id: Uuid::now_v7(),
        event_type: event_type.to_owned(),
        entity_type: "signal".to_owned(),
        entity_id: Some(Uuid::now_v7()),
        payload: json!({"symbol": "XAUUSDT"}),
        created_at: 1_782_195_600_000,
    }
}

fn tracker_state() -> TrackerState {
    TrackerState {
        signal_id: Uuid::now_v7(),
        symbol: Symbol::new("XAUUSDT").unwrap(),
        state: json!({"setupState": "ENTRY_AVAILABLE", "tradeState": "NOT_ENTERED"}),
        tracking_health: TrackingHealth::Live,
    }
}
