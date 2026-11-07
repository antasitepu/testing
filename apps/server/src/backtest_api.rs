use crate::{dto::ApiErrorDto, state::AppState};
use analysis_domain::PlanKind;
use analysis_service::{AnalysisServiceError, BacktestRunInput, BacktestRunRecord};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use market_domain::Timeframe;
use serde::Serialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/backtests", post(create_backtest))
        .route("/api/v1/backtests/{run_id}", get(get_backtest))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreatedBacktest {
    run_id: Uuid,
}

async fn create_backtest(
    State(state): State<AppState>,
    Json(input): Json<BacktestRunInput>,
) -> Result<(StatusCode, Json<CreatedBacktest>), BacktestApiError> {
    validate_input(&input)?;
    let backtests = state.backtests.ok_or_else(BacktestApiError::unavailable)?;
    let configurations = state
        .configurations
        .ok_or_else(BacktestApiError::unavailable)?;
    let profiles = configurations
        .profiles_for_release(input.release_id, &input.symbol)
        .await
        .map_err(BacktestApiError::from_service)?;
    if profiles.is_empty() {
        return Err(BacktestApiError::not_found_release());
    }
    let profile = profiles
        .iter()
        .find(|profile| {
            profile.key.plan == input.plan
                && profile.key.target_timeframe == input.target_timeframe
                && profile.versions.algorithm_version == input.algorithm_version
                && profile.versions.parameter_version == input.parameter_version
        })
        .ok_or_else(|| BacktestApiError::invalid("matching profile version was not found"))?;
    if !profile.enabled {
        return Err(BacktestApiError::invalid("profile is disabled"));
    }

    let run_id = Uuid::now_v7();
    let record = BacktestRunRecord::new(run_id, input).map_err(BacktestApiError::from_service)?;
    backtests
        .create_run(record)
        .await
        .map_err(BacktestApiError::from_service)?;
    Ok((StatusCode::ACCEPTED, Json(CreatedBacktest { run_id })))
}

async fn get_backtest(
    State(state): State<AppState>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<analysis_service::BacktestRunView>, BacktestApiError> {
    let backtests = state.backtests.ok_or_else(BacktestApiError::unavailable)?;
    let run = backtests
        .run(run_id)
        .await
        .map_err(BacktestApiError::from_service)?
        .ok_or_else(BacktestApiError::not_found_backtest)?;
    Ok(Json(run))
}

fn validate_input(input: &BacktestRunInput) -> Result<(), BacktestApiError> {
    if input.start_time > input.end_time {
        return Err(BacktestApiError::invalid(
            "startTime must be before endTime",
        ));
    }
    if input.algorithm_version.trim().is_empty()
        || input.parameter_version.trim().is_empty()
        || input.code_commit_hash.trim().is_empty()
    {
        return Err(BacktestApiError::invalid(
            "algorithmVersion, parameterVersion, and codeCommitHash are required",
        ));
    }
    if input.plan == PlanKind::MeanReversion && input.target_timeframe == Timeframe::D1 {
        return Err(BacktestApiError::invalid(
            "mean reversion daily backtest is not enabled by default",
        ));
    }
    Ok(())
}

struct BacktestApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl BacktestApiError {
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
            code: "backtest_unavailable",
            message: "backtest service is unavailable".to_owned(),
        }
    }

    fn not_found_release() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "release_not_found",
            message: "matching backtest release profile was not found".to_owned(),
        }
    }

    fn not_found_backtest() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "backtest_not_found",
            message: "backtest run was not found".to_owned(),
        }
    }

    fn from_service(error: AnalysisServiceError) -> Self {
        match error {
            AnalysisServiceError::InvalidData(message) => Self::invalid(message),
            AnalysisServiceError::Conflict(message) => Self {
                status: StatusCode::CONFLICT,
                code: "conflict",
                message,
            },
            other => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "backtest_unavailable",
                message: other.to_string(),
            },
        }
    }
}

impl IntoResponse for BacktestApiError {
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
