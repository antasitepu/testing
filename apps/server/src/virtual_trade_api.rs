use crate::{dto::ApiErrorDto, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use execution_service::{ExecutionError, VirtualTradeDetail, VirtualTradeHistoryQuery};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/virtual-trades", get(history))
        .route("/api/v1/virtual-trades/{trade_id}", get(detail))
}

#[derive(Debug)]
struct VirtualTradeApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl VirtualTradeApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "virtual_trade_unavailable",
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "virtual trade was not found".to_owned(),
        }
    }
}

impl IntoResponse for VirtualTradeApiError {
    fn into_response(self) -> Response {
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
#[serde(rename_all = "camelCase")]
struct HistoryQuery {
    symbol: Option<String>,
    plan: Option<String>,
    interval: Option<String>,
    direction: Option<String>,
    exit_reason: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    cursor: Option<i64>,
    limit: Option<u32>,
}

async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<execution_service::VirtualTradeHistoryPage>, VirtualTradeApiError> {
    let store = state.virtual_trades.ok_or_else(|| {
        VirtualTradeApiError::unavailable("virtual trade store is not configured")
    })?;
    let limit = query.limit.unwrap_or(100);
    if limit == 0 || limit > 500 {
        return Err(VirtualTradeApiError::invalid(
            "virtual trade limit must be 1..=500",
        ));
    }
    store
        .history(VirtualTradeHistoryQuery {
            symbol: query.symbol,
            plan: query.plan,
            timeframe: query.interval,
            direction: query.direction,
            exit_reason: query.exit_reason,
            from: query.from,
            to: query.to,
            cursor: query.cursor,
            limit,
        })
        .await
        .map(Json)
        .map_err(map_execution_error)
}

async fn detail(
    State(state): State<AppState>,
    Path(trade_id): Path<Uuid>,
) -> Result<Json<VirtualTradeDetail>, VirtualTradeApiError> {
    let store = state.virtual_trades.ok_or_else(|| {
        VirtualTradeApiError::unavailable("virtual trade store is not configured")
    })?;
    store
        .detail(trade_id)
        .await
        .map_err(map_execution_error)?
        .map(Json)
        .ok_or_else(VirtualTradeApiError::not_found)
}

fn map_execution_error(error: ExecutionError) -> VirtualTradeApiError {
    VirtualTradeApiError::unavailable(error.to_string())
}
