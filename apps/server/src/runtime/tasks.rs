use super::health::RuntimeHealth;
use analysis_service::AnalysisServiceError;
use execution_service::ExecutionError;
use futures_util::FutureExt;
use market_service::ServiceError;
use std::{fmt::Display, future::Future, panic::AssertUnwindSafe, time::Duration};
use tokio::task::JoinHandle;

/// Publishes a panicking background task as a task failure right away instead of
/// letting it disappear silently until shutdown, then re-raises the panic.
pub async fn publish_panic<F>(health: &RuntimeHealth, name: &'static str, task: F) -> F::Output
where
    F: Future,
{
    match AssertUnwindSafe(task).catch_unwind().await {
        Ok(output) => output,
        Err(payload) => {
            health.publish_task_failure(format!("{name} panicked"), super::current_unix_millis());
            std::panic::resume_unwind(payload)
        }
    }
}

async fn stop_task<E>(name: &'static str, task: &mut JoinHandle<Result<(), E>>)
where
    E: Display,
{
    match tokio::time::timeout(Duration::from_secs(10), &mut *task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::error!(task = name, %error, "background task failed"),
        Ok(Err(error)) => tracing::error!(task = name, %error, "background task panicked"),
        Err(_) => {
            tracing::warn!(task = name, "background task exceeded shutdown deadline");
            task.abort();
            let _ = task.await;
        }
    }
}

pub async fn stop_service_task(
    name: &'static str,
    task: &mut JoinHandle<Result<(), ServiceError>>,
) {
    stop_task(name, task).await;
}

pub async fn stop_analysis_task(
    name: &'static str,
    task: &mut JoinHandle<Result<(), AnalysisServiceError>>,
) {
    stop_task(name, task).await;
}

pub async fn stop_execution_task(
    name: &'static str,
    task: &mut JoinHandle<Result<(), ExecutionError>>,
) {
    stop_task(name, task).await;
}

#[cfg(test)]
mod tests {
    use super::publish_panic;
    use crate::runtime::health::{RuntimeDiagnosticsSnapshot, RuntimeHealth, RuntimePhase};

    #[tokio::test]
    async fn panicking_task_is_published_as_failure() {
        let health = RuntimeHealth::new(RuntimeDiagnosticsSnapshot::empty(), 0);
        let task_health = health.clone();
        let joined = tokio::spawn(async move {
            publish_panic(&task_health, "analysis worker", async { panic!("boom") }).await
        })
        .await;

        assert!(joined.unwrap_err().is_panic());
        let snapshot = health.snapshot();
        assert_eq!(snapshot.phase, RuntimePhase::Degraded);
        assert_eq!(snapshot.reason.as_deref(), Some("analysis worker panicked"));
    }
}
