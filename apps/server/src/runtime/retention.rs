use execution_service::{ExecutionError, MonitoringStore, OutboxRetention};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

/// Published outbox rows are kept 30 days; pending or failed rows are never pruned.
const OUTBOX_RETENTION: OutboxRetention = OutboxRetention::new(30, 500);
const OUTBOX_RETENTION_BATCH: u32 = 500;
const OUTBOX_RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(super) async fn run_outbox_retention(
    store: Arc<dyn MonitoringStore>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(OUTBOX_RETENTION_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = interval.tick() => {
                match prune_outbox(store.as_ref(), super::current_unix_millis()).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, "pruned published monitoring events");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "monitoring outbox retention failed");
                    }
                }
            }
        }
    }
}

async fn prune_outbox(store: &dyn MonitoringStore, now_ms: i64) -> Result<u64, ExecutionError> {
    let cutoff = now_ms.saturating_sub(OUTBOX_RETENTION.cutoff_ms());
    let mut total = 0;
    loop {
        let deleted = store
            .cleanup_published_before(cutoff, OUTBOX_RETENTION_BATCH)
            .await?;
        total += deleted;
        if deleted < u64::from(OUTBOX_RETENTION_BATCH) {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OUTBOX_RETENTION, prune_outbox};
    use async_trait::async_trait;
    use execution_service::{
        ExecutionError, MonitoringSnapshot, MonitoringStore, OutboxStreamBatch,
    };
    use std::sync::Mutex;

    struct FakeStore {
        batches: Mutex<Vec<u64>>,
        cutoffs: Mutex<Vec<i64>>,
    }

    #[async_trait]
    impl MonitoringStore for FakeStore {
        async fn snapshot(&self) -> Result<MonitoringSnapshot, ExecutionError> {
            unreachable!("retention never reads snapshots")
        }

        async fn events_after(
            &self,
            _cursor: i64,
            _limit: u32,
        ) -> Result<OutboxStreamBatch, ExecutionError> {
            unreachable!("retention never streams events")
        }

        async fn mark_published(&self, _cursor: i64) -> Result<(), ExecutionError> {
            unreachable!("retention never publishes events")
        }

        async fn cleanup_published_before(
            &self,
            older_than_ms: i64,
            _limit: u32,
        ) -> Result<u64, ExecutionError> {
            self.cutoffs.lock().unwrap().push(older_than_ms);
            Ok(self.batches.lock().unwrap().remove(0))
        }
    }

    #[tokio::test]
    async fn prune_repeats_full_batches_with_thirty_day_cutoff() {
        let store = FakeStore {
            batches: Mutex::new(vec![500, 500, 3]),
            cutoffs: Mutex::new(Vec::new()),
        };
        let now = OUTBOX_RETENTION.cutoff_ms() + 1_000;

        assert_eq!(prune_outbox(&store, now).await.unwrap(), 1_003);
        assert_eq!(*store.cutoffs.lock().unwrap(), vec![1_000, 1_000, 1_000]);
    }
}
