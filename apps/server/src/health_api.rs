use crate::{
    runtime::health::{RuntimeHealthSnapshot, RuntimePhase},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/health/diagnostics", get(diagnostics))
}

#[derive(Serialize)]
struct HealthStatusDto {
    phase: RuntimePhase,
    reason: Option<String>,
}

impl From<RuntimeHealthSnapshot> for HealthStatusDto {
    fn from(snapshot: RuntimeHealthSnapshot) -> Self {
        Self {
            phase: snapshot.phase,
            reason: snapshot.reason,
        }
    }
}

async fn live(State(state): State<AppState>) -> Json<HealthStatusDto> {
    Json(state.runtime_health.snapshot().into())
}

async fn ready(State(state): State<AppState>) -> Response {
    let snapshot = state.runtime_health.snapshot();
    let status = if snapshot.phase == RuntimePhase::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(HealthStatusDto::from(snapshot))).into_response()
}

async fn diagnostics(State(state): State<AppState>) -> Json<RuntimeHealthSnapshot> {
    Json(state.runtime_health.snapshot())
}
