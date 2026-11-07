use crate::{dto::ApiErrorDto, state::AppState};
use analysis_domain::{PlanKind, PlanProfileKey};
use analysis_service::{AnalysisServiceError, SignalHistoryQuery};
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
use futures_util::{Stream, stream};
use market_domain::{Symbol, Timeframe};
use serde::Deserialize;
use std::{collections::VecDeque, convert::Infallible, str::FromStr, time::Duration};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/analysis/latest", get(latest))
        .route("/api/v1/signals", get(signals))
        .route("/api/v1/analysis-stream", get(analysis_stream))
}

#[derive(Deserialize)]
struct PlanQuery {
    symbol: String,
    interval: String,
    plan: String,
}

#[derive(Deserialize)]
struct HistoryQuery {
    symbol: String,
    interval: String,
    plan: String,
    before: Option<i64>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct AnalysisStreamQuery {
    cursor: Option<i64>,
}

async fn latest(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<analysis_domain::CurrentPlanProjection>, AnalysisApiError> {
    let store = state.analysis.ok_or_else(AnalysisApiError::unavailable)?;
    let key = key(&query.symbol, &query.interval, &query.plan)?;
    store
        .active_projection(&key)
        .await
        .map(Json)
        .map_err(AnalysisApiError::from_service)
}

async fn signals(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<analysis_service::SignalPage>, AnalysisApiError> {
    let store = state.analysis.ok_or_else(AnalysisApiError::unavailable)?;
    let key = key(&query.symbol, &query.interval, &query.plan)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=1_000).contains(&limit) {
        return Err(AnalysisApiError::invalid(
            "limit must be between 1 and 1000",
        ));
    }
    store
        .history(SignalHistoryQuery {
            key,
            before_created_at: query.before,
            limit,
        })
        .await
        .map(Json)
        .map_err(AnalysisApiError::from_service)
}

async fn analysis_stream(
    State(state): State<AppState>,
    Query(query): Query<AnalysisStreamQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AnalysisApiError> {
    let store = state.analysis.ok_or_else(AnalysisApiError::unavailable)?;
    let cursor = query.cursor.unwrap_or(0);
    if cursor < 0 {
        return Err(AnalysisApiError::invalid(
            "cursor must be a non-negative integer",
        ));
    }
    let cursor = crate::monitoring_api::resume_cursor(&headers, cursor);
    let events = stream::unfold(
        (store, cursor, VecDeque::new()),
        |(store, mut cursor, mut pending)| async move {
            loop {
                // Drain the fetched batch before querying again.
                if let Some(record) = pending.pop_front() {
                    let record: analysis_service::MonitoringEventRecord = record;
                    cursor = record.cursor;
                    let event = Event::default()
                        .id(record.cursor.to_string())
                        .event(event_name(&record.event.event_type))
                        .json_data(record.event)
                        .unwrap_or_else(|_| Event::default().event("resync").data("{}"));
                    return Some((Ok(event), (store, cursor, pending)));
                }
                match store.monitoring_events_after(cursor, 100).await {
                    Ok(records) if records.is_empty() => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Ok(records) => pending.extend(records),
                    Err(error) => {
                        let event = Event::default().event("resync").data(format!(
                            "{{\"reason\":{}}}",
                            json_string(&error.to_string())
                        ));
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        return Some((Ok(event), (store, cursor, pending)));
                    }
                }
            }
        },
    );
    Ok(Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

fn key(symbol: &str, interval: &str, plan: &str) -> Result<PlanProfileKey, AnalysisApiError> {
    Ok(PlanProfileKey {
        symbol: Symbol::new(symbol)
            .map_err(|error| AnalysisApiError::invalid(error.to_string()))?,
        target_timeframe: Timeframe::from_str(interval)
            .map_err(|error| AnalysisApiError::invalid(error.to_string()))?,
        plan: parse_plan(plan)?,
    })
}

fn parse_plan(value: &str) -> Result<PlanKind, AnalysisApiError> {
    match value {
        "ICT" => Ok(PlanKind::Ict),
        "SMC" => Ok(PlanKind::Smc),
        "RSI" => Ok(PlanKind::Rsi),
        "QUASIMODO" => Ok(PlanKind::Quasimodo),
        "TREND_BREAKOUT" => Ok(PlanKind::TrendBreakout),
        "MEAN_REVERSION" => Ok(PlanKind::MeanReversion),
        _ => Err(AnalysisApiError::invalid("unknown plan")),
    }
}

fn event_name(value: &str) -> &'static str {
    match value {
        "PROJECTION" => "projection",
        "SIGNAL" => "signal",
        "SIGNAL_EVENT" => "signal-event",
        "DIRECTIVE" => "directive",
        _ => "resync",
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"serialization error\"".to_owned())
}

struct AnalysisApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl AnalysisApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "analysis_unavailable",
            message: "analysis store is unavailable".to_owned(),
        }
    }

    fn from_service(error: AnalysisServiceError) -> Self {
        let invalid = matches!(error, AnalysisServiceError::InvalidData(_));
        Self {
            status: if invalid {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            code: if invalid {
                "invalid_request"
            } else {
                "analysis_unavailable"
            },
            message: error.to_string(),
        }
    }
}

impl IntoResponse for AnalysisApiError {
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
