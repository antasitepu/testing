use crate::{dto::ApiErrorDto, state::AppState};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use execution_service::{ExecutionError, MonitoringSnapshot, MonitoringStore};
use futures_util::{Stream, stream};
use serde::Deserialize;
use std::{collections::VecDeque, convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::watch;

const MONITORING_LEGACY_POLL: Duration = Duration::from_millis(750);
const MONITORING_WAKE_FALLBACK: Duration = Duration::from_secs(10);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/monitoring/snapshot", get(snapshot))
        .route("/api/v1/monitoring-stream", get(stream_events))
}

#[derive(Debug)]
struct MonitoringApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl MonitoringApiError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "monitoring_unavailable",
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }
}

impl IntoResponse for MonitoringApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(ApiErrorDto {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    after: Option<i64>,
}

async fn snapshot(
    State(state): State<AppState>,
) -> Result<Json<MonitoringSnapshot>, MonitoringApiError> {
    let store = state
        .monitoring
        .ok_or_else(|| MonitoringApiError::unavailable("monitoring store is not configured"))?;
    store
        .snapshot()
        .await
        .map(Json)
        .map_err(map_execution_error)
}

async fn stream_events(
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, MonitoringApiError> {
    let after = query.after.unwrap_or(0);
    if after < -1 {
        return Err(MonitoringApiError::invalid(
            "monitoring cursor must be -1 or greater",
        ));
    }
    let after = resume_cursor(&headers, after);
    let store = state
        .monitoring
        .ok_or_else(|| MonitoringApiError::unavailable("monitoring store is not configured"))?;
    let wake = state
        .monitoring_wake
        .as_ref()
        .map(|source| source.subscribe());
    Ok(
        Sse::new(monitoring_event_stream(store, after, wake)).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        ),
    )
}

/// EventSource reconnects with the URL it was opened with; `Last-Event-ID` carries
/// the newest cursor the client already received, so resume from there instead of
/// replaying the backlog since the original URL cursor.
pub(crate) fn resume_cursor(headers: &HeaderMap, requested: i64) -> i64 {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|cursor| *cursor >= 0)
        .map_or(requested, |cursor| cursor.max(requested))
}

struct MonitoringStreamState {
    store: Arc<dyn MonitoringStore>,
    cursor: i64,
    pending: VecDeque<Event>,
    wake: Option<watch::Receiver<u64>>,
}

fn monitoring_event_stream(
    store: Arc<dyn MonitoringStore>,
    after: i64,
    wake: Option<watch::Receiver<u64>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(
        MonitoringStreamState {
            store,
            cursor: after,
            pending: VecDeque::new(),
            wake,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                match state.store.events_after(state.cursor, 100).await {
                    Ok(batch) if batch.events.is_empty() => {
                        wait_for_monitoring_wake(&mut state.wake).await;
                    }
                    Ok(batch) if batch.resync_required => {
                        // Emit one synthetic resync, then park this connection.
                        // Clients must re-snapshot and reopen with a fresh after=
                        // cursor; re-polling the same after would thrash forever.
                        for event in batch.events {
                            let payload = serde_json::to_string(&event.payload)
                                .unwrap_or_else(|_| "{}".to_owned());
                            state.pending.push_back(
                                Event::default()
                                    .id(event.cursor.to_string())
                                    .event(event.event_type)
                                    .data(payload),
                            );
                        }
                        state.cursor = i64::MAX;
                    }
                    Ok(batch) => {
                        for event in batch.events {
                            let payload = serde_json::to_string(&event.payload)
                                .unwrap_or_else(|_| "{}".to_owned());
                            let _ = state.store.mark_published(event.cursor).await;
                            state.cursor = event.cursor;
                            state.pending.push_back(
                                Event::default()
                                    .id(event.cursor.to_string())
                                    .event(event.event_type)
                                    .data(payload),
                            );
                        }
                    }
                    Err(error) => {
                        state.pending.push_back(
                            Event::default()
                                .id(state.cursor.max(0).to_string())
                                .event("health")
                                .data(
                                    serde_json::json!({
                                        "symbol": "XAUUSDT",
                                        "health": "STALE",
                                        "reason": error.to_string()
                                    })
                                    .to_string(),
                                ),
                        );
                        tokio::time::sleep(MONITORING_LEGACY_POLL).await;
                    }
                }
            }
        },
    )
}

async fn wait_for_monitoring_wake(wake: &mut Option<watch::Receiver<u64>>) {
    let Some(receiver) = wake else {
        tokio::time::sleep(MONITORING_LEGACY_POLL).await;
        return;
    };
    tokio::select! {
        _ = receiver.changed() => {}
        _ = tokio::time::sleep(MONITORING_WAKE_FALLBACK) => {}
    }
}

fn map_execution_error(error: ExecutionError) -> MonitoringApiError {
    MonitoringApiError::unavailable(error.to_string())
}
