use market_domain::Timeframe;
use serde::Serialize;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimePhase {
    Starting,
    CatchingUp,
    Ready,
    Degraded,
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePoolSnapshot {
    pub size: u32,
    pub idle_connections: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeTimeframeSnapshot {
    pub timeframe: Timeframe,
    pub latest_synced_time: Option<i64>,
    pub latest_closed_candle_time: Option<i64>,
    pub latest_evaluation_time: Option<i64>,
    pub analysis_backlog: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCounterSnapshot {
    pub recovery_count: u64,
    pub task_failure_count: u64,
    pub market_reconnect_count: u64,
    pub market_resync_count: u64,
    pub execution_exact_replay_recovery_count: u64,
    pub execution_ohlc_reconciled_recovery_count: u64,
    pub subscriber_lag_count: u64,
    pub db_acquire_timeout_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDiagnosticsSnapshot {
    pub pool: RuntimePoolSnapshot,
    pub timeframes: Vec<RuntimeTimeframeSnapshot>,
    pub counters: RuntimeCounterSnapshot,
    pub active_recommendation_count: u64,
    pub active_trade_count: u64,
    pub latest_market_activity: Option<i64>,
    pub latest_analysis_activity: Option<i64>,
    pub latest_execution_activity: Option<i64>,
}

impl RuntimeDiagnosticsSnapshot {
    pub fn empty() -> Self {
        Self {
            pool: RuntimePoolSnapshot {
                size: 0,
                idle_connections: 0,
            },
            timeframes: Vec::new(),
            counters: RuntimeCounterSnapshot::default(),
            active_recommendation_count: 0,
            active_trade_count: 0,
            latest_market_activity: None,
            latest_analysis_activity: None,
            latest_execution_activity: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeHealthSnapshot {
    pub phase: RuntimePhase,
    pub reason: Option<String>,
    pub updated_at: i64,
    pub diagnostics: RuntimeDiagnosticsSnapshot,
}

#[derive(Clone)]
pub struct RuntimeHealth {
    sender: watch::Sender<RuntimeHealthSnapshot>,
}

#[derive(Clone)]
pub struct RuntimeHealthReader {
    receiver: watch::Receiver<RuntimeHealthSnapshot>,
}

impl RuntimeHealth {
    pub fn new(diagnostics: RuntimeDiagnosticsSnapshot, updated_at: i64) -> Self {
        let (sender, _) = watch::channel(RuntimeHealthSnapshot {
            phase: RuntimePhase::Starting,
            reason: None,
            updated_at,
            diagnostics,
        });
        Self { sender }
    }

    pub fn snapshot(&self) -> RuntimeHealthSnapshot {
        self.sender.borrow().clone()
    }

    pub fn publish(&self, phase: RuntimePhase, reason: Option<String>, updated_at: i64) {
        self.sender.send_modify(|snapshot| {
            snapshot.phase = phase;
            snapshot.reason = reason;
            snapshot.updated_at = updated_at;
        });
    }

    pub fn publish_diagnostics(
        &self,
        phase: RuntimePhase,
        reason: Option<String>,
        mut diagnostics: RuntimeDiagnosticsSnapshot,
        updated_at: i64,
    ) {
        self.sender.send_modify(|snapshot| {
            diagnostics.counters.recovery_count = snapshot.diagnostics.counters.recovery_count;
            diagnostics.counters.task_failure_count =
                snapshot.diagnostics.counters.task_failure_count;
            if !matches!(
                snapshot.phase,
                RuntimePhase::Degraded | RuntimePhase::Stopping
            ) {
                snapshot.phase = phase;
                snapshot.reason = reason;
            }
            snapshot.updated_at = updated_at;
            snapshot.diagnostics = diagnostics;
        });
    }

    pub fn publish_readiness_failure(&self, reason: String, updated_at: i64) {
        self.sender.send_modify(|snapshot| {
            if !matches!(
                snapshot.phase,
                RuntimePhase::Degraded | RuntimePhase::Stopping
            ) {
                snapshot.phase = RuntimePhase::CatchingUp;
                snapshot.reason = Some(reason);
                snapshot.updated_at = updated_at;
            }
        });
    }

    pub fn increment_recovery_count(&self, updated_at: i64) {
        self.sender.send_modify(|snapshot| {
            snapshot.diagnostics.counters.recovery_count = snapshot
                .diagnostics
                .counters
                .recovery_count
                .saturating_add(1);
            snapshot.updated_at = updated_at;
        });
    }

    pub fn publish_task_failure(&self, reason: String, updated_at: i64) {
        self.sender.send_modify(|snapshot| {
            if snapshot.phase != RuntimePhase::Stopping {
                snapshot.phase = RuntimePhase::Degraded;
                snapshot.reason = Some(reason);
            }
            snapshot.updated_at = updated_at;
            snapshot.diagnostics.counters.task_failure_count = snapshot
                .diagnostics
                .counters
                .task_failure_count
                .saturating_add(1);
        });
    }

    pub fn subscribe(&self) -> RuntimeHealthReader {
        RuntimeHealthReader {
            receiver: self.sender.subscribe(),
        }
    }
}

impl RuntimeHealthReader {
    pub fn starting(updated_at: i64) -> Self {
        RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), updated_at).subscribe()
    }

    pub fn snapshot(&self) -> RuntimeHealthSnapshot {
        self.receiver.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn runtime_health_starts_in_starting_and_publishes_changes() {
        let health = RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), 100);
        assert_eq!(health.snapshot().phase, RuntimePhase::Starting);

        health.publish(
            RuntimePhase::CatchingUp,
            Some("analysis backlog".into()),
            200,
        );

        assert_eq!(health.snapshot().phase, RuntimePhase::CatchingUp);
        assert_eq!(
            health.snapshot().reason.as_deref(),
            Some("analysis backlog")
        );
    }

    #[test]
    fn concurrent_readiness_update_preserves_degraded_failure_and_latest_counters() {
        let health = RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), 100);
        let stale_diagnostics = health.snapshot().diagnostics;
        let readiness_health = health.clone();
        let (release_readiness, wait_for_failure) = mpsc::sync_channel(0);

        let readiness = thread::spawn(move || {
            wait_for_failure.recv().expect("release readiness update");
            readiness_health.publish_diagnostics(RuntimePhase::Ready, None, stale_diagnostics, 400);
        });

        health.increment_recovery_count(200);
        health.publish_task_failure("analysis worker failed".to_owned(), 300);
        release_readiness
            .send(())
            .expect("release readiness after failure");
        readiness.join().expect("readiness thread");

        let snapshot = health.snapshot();
        assert_eq!(snapshot.phase, RuntimePhase::Degraded);
        assert_eq!(snapshot.reason.as_deref(), Some("analysis worker failed"));
        assert_eq!(snapshot.diagnostics.counters.recovery_count, 1);
        assert_eq!(snapshot.diagnostics.counters.task_failure_count, 1);
    }

    #[test]
    fn concurrent_readiness_update_preserves_stopping_and_latest_counters() {
        let health = RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), 100);
        let stale_diagnostics = health.snapshot().diagnostics;
        let readiness_health = health.clone();
        let (release_readiness, wait_for_stopping) = mpsc::sync_channel(0);

        let readiness = thread::spawn(move || {
            wait_for_stopping.recv().expect("release readiness update");
            readiness_health.publish_diagnostics(
                RuntimePhase::CatchingUp,
                Some("analysis backlog".to_owned()),
                stale_diagnostics,
                400,
            );
        });

        health.publish(RuntimePhase::Stopping, None, 200);
        health.increment_recovery_count(300);
        release_readiness
            .send(())
            .expect("release readiness after stopping");
        readiness.join().expect("readiness thread");

        let snapshot = health.snapshot();
        assert_eq!(snapshot.phase, RuntimePhase::Stopping);
        assert_eq!(snapshot.reason, None);
        assert_eq!(snapshot.diagnostics.counters.recovery_count, 1);
        assert_eq!(snapshot.diagnostics.counters.task_failure_count, 0);
    }

    #[test]
    fn diagnostics_refresh_preserves_lifecycle_counters_and_accepts_observability_counters() {
        let health = RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), 100);
        health.increment_recovery_count(200);
        let mut diagnostics = RuntimeDiagnosticsSnapshot::empty();
        diagnostics.counters.market_reconnect_count = 3;
        diagnostics.counters.db_acquire_timeout_count = 2;

        health.publish_diagnostics(RuntimePhase::Ready, None, diagnostics, 300);

        let counters = health.snapshot().diagnostics.counters;
        assert_eq!(counters.recovery_count, 1);
        assert_eq!(counters.market_reconnect_count, 3);
        assert_eq!(counters.db_acquire_timeout_count, 2);
    }
}
