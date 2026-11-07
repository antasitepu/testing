use analysis_domain::{AnalysisWorkItem, PlanProfileKey, StructuredAnalysisError};
use analysis_service::{
    AnalysisServiceError, AnalysisWorkStore, EvaluationCommit, PlanRuntimeStateRecord,
};
use async_trait::async_trait;
use execution_service::{
    DurableTransition, ExecutionError, HealthTransition, MarketCursor, MonitoringEvent,
    TrackerSnapshot, TrackerStore,
};
use market_domain::Symbol;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct ExecutionWakeSource {
    sender: watch::Sender<u64>,
}

impl ExecutionWakeSource {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(0);
        Self { sender }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.sender.subscribe()
    }

    pub fn wake(&self) {
        self.sender.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }
}

impl Default for ExecutionWakeSource {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ExecutionBarrierRequest {
    pub close_time: i64,
    /// Whether execution reached `close_time`. If not, the work must wait and try again.
    pub done: oneshot::Sender<bool>,
}

/// Keeps analysis and execution in step: analysis waits until execution has reached a bar before
/// evaluating it, so every evaluation sees the trade state of that moment in time.
#[derive(Clone)]
pub struct ExecutionBarrier {
    requests: mpsc::Sender<ExecutionBarrierRequest>,
}

impl ExecutionBarrier {
    pub fn channel() -> (Self, mpsc::Receiver<ExecutionBarrierRequest>) {
        let (requests, receiver) = mpsc::channel(1);
        (Self { requests }, receiver)
    }

    async fn reach(&self, close_time: i64) -> Result<bool, AnalysisServiceError> {
        let (done, finished) = oneshot::channel();
        self.requests
            .send(ExecutionBarrierRequest { close_time, done })
            .await
            .map_err(|_| execution_stopped())?;
        finished.await.map_err(|_| execution_stopped())
    }
}

fn execution_stopped() -> AnalysisServiceError {
    AnalysisServiceError::Storage {
        operation: "reach execution barrier",
        message: "execution supervisor stopped".to_owned(),
    }
}

pub struct NotifyingAnalysisWorkStore {
    inner: Arc<dyn AnalysisWorkStore>,
    execution_wake: ExecutionWakeSource,
    monitoring_wake: Option<ExecutionWakeSource>,
    execution_barrier: Option<ExecutionBarrier>,
}

impl NotifyingAnalysisWorkStore {
    pub fn new(inner: Arc<dyn AnalysisWorkStore>, execution_wake: ExecutionWakeSource) -> Self {
        Self {
            inner,
            execution_wake,
            monitoring_wake: None,
            execution_barrier: None,
        }
    }

    pub fn with_execution_barrier(mut self, execution_barrier: ExecutionBarrier) -> Self {
        self.execution_barrier = Some(execution_barrier);
        self
    }

    pub fn with_monitoring_wake(mut self, monitoring_wake: ExecutionWakeSource) -> Self {
        self.monitoring_wake = Some(monitoring_wake);
        self
    }
}

#[async_trait]
impl AnalysisWorkStore for NotifyingAnalysisWorkStore {
    async fn lease_next(
        &self,
        owner: &str,
        now_ms: i64,
    ) -> Result<Option<AnalysisWorkItem>, AnalysisServiceError> {
        let work = self.inner.lease_next(owner, now_ms).await?;
        if let (Some(leased), Some(barrier)) = (&work, &self.execution_barrier)
            && !barrier.reach(leased.canonical_close_time).await?
        {
            self.inner
                .defer(leased.work_id, leased.attempt_count)
                .await?;
            return Ok(None);
        }
        Ok(work)
    }

    async fn complete_evaluation(
        &self,
        commit: EvaluationCommit,
    ) -> Result<(), AnalysisServiceError> {
        let should_wake_execution = !commit.signals.is_empty() || !commit.directives.is_empty();
        let should_wake_monitoring = !commit.monitoring_events.is_empty();
        self.inner.complete_evaluation(commit).await?;
        if should_wake_execution {
            self.execution_wake.wake();
        }
        if should_wake_monitoring && let Some(wake) = &self.monitoring_wake {
            wake.wake();
        }
        Ok(())
    }

    async fn runtime_state(
        &self,
        key: &PlanProfileKey,
        release_id: Uuid,
    ) -> Result<Option<PlanRuntimeStateRecord>, AnalysisServiceError> {
        self.inner.runtime_state(key, release_id).await
    }

    async fn defer(&self, work_id: Uuid, lease_attempt: u32) -> Result<(), AnalysisServiceError> {
        self.inner.defer(work_id, lease_attempt).await
    }

    async fn fail(
        &self,
        work_id: Uuid,
        lease_attempt: u32,
        error: StructuredAnalysisError,
    ) -> Result<(), AnalysisServiceError> {
        self.inner.fail(work_id, lease_attempt, error).await
    }
}

pub struct NotifyingTrackerStore {
    inner: Arc<dyn TrackerStore>,
    monitoring_wake: ExecutionWakeSource,
}

impl NotifyingTrackerStore {
    pub fn new(inner: Arc<dyn TrackerStore>, monitoring_wake: ExecutionWakeSource) -> Self {
        Self {
            inner,
            monitoring_wake,
        }
    }
}

#[async_trait]
impl TrackerStore for NotifyingTrackerStore {
    async fn load_active(&self, symbol: &Symbol) -> Result<TrackerSnapshot, ExecutionError> {
        self.inner.load_active(symbol).await
    }

    async fn checkpoint_cursor(&self, cursor: MarketCursor) -> Result<(), ExecutionError> {
        self.inner.checkpoint_cursor(cursor).await
    }

    async fn commit_transition(
        &self,
        transition: DurableTransition,
    ) -> Result<Option<MonitoringEvent>, ExecutionError> {
        let event = self.inner.commit_transition(transition).await?;
        // Always wake: write-only commits (MFE/MAE) may have no monitoring row
        // but still change durable state observers care about.
        self.monitoring_wake.wake();
        Ok(event)
    }

    async fn mark_health(
        &self,
        change: HealthTransition,
    ) -> Result<MonitoringEvent, ExecutionError> {
        let event = self.inner.mark_health(change).await?;
        self.monitoring_wake.wake();
        Ok(event)
    }
}
