use crate::{dto::ApiErrorDto, runtime_profiles, state::AppState};
use analysis_service::{
    AnalysisServiceError, MaterializedProfile, RejectedPlanCandidate, RejectedPlanCandidateQuery,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use execution_service::{
    ExecutionError, MonitoringEventEnvelope, NewScalpingAuditSnapshot, ScalpingAuditSnapshot,
    ScalpingAuditSnapshotPage, ScalpingAuditSnapshotQuery, VirtualTradeHistoryQuery,
};
use market_domain::{Symbol, Timeframe};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/runtime/strategy-summary", get(strategy_summary))
        .route(
            "/api/v1/runtime/scalping-dashboard",
            get(scalping_dashboard),
        )
        .route(
            "/api/v1/runtime/scalping-audit-snapshots",
            get(list_scalping_audit_snapshots).post(create_scalping_audit_snapshot),
        )
        .route(
            "/api/v1/runtime/scalping-audit-comparison",
            get(scalping_audit_comparison),
        )
}

const SCALPING_TIMEFRAMES: [&str; 3] = ["1m", "5m", "15m"];
const REJECTED_CANDIDATE_DEBUG_LIMIT: u32 = 50;

#[derive(Deserialize)]
struct StrategySummaryQuery {
    symbol: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingDashboardQuery {
    symbol: String,
    journal_limit: Option<u32>,
    trade_limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingAuditSnapshotListQuery {
    symbol: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug)]
struct RuntimeApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl RuntimeApiError {
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
            code: "runtime_summary_unavailable",
            message: message.into(),
        }
    }
}

impl IntoResponse for RuntimeApiError {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStrategySummaryDto {
    symbol: String,
    release_id: Option<Uuid>,
    total_profiles: usize,
    locked_profiles: usize,
    active_overlay_count: usize,
    open_trade_count: usize,
    decisions: Vec<RuntimeDecisionDto>,
    rows: Vec<RuntimeStrategyRowDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingDashboardDto {
    symbol: String,
    mode: &'static str,
    timeframes: Vec<&'static str>,
    report: ScalpingReportDto,
    journal: ForwardJournalDto,
    debug: SignalDebugDto,
    audit: LowPerformerAuditDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingReportDto {
    total_profiles: usize,
    locked_profiles: usize,
    baseline_profiles: usize,
    active_overlay_count: usize,
    open_trade_count: usize,
    rows: Vec<RuntimeStrategyRowDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ForwardJournalDto {
    latest_cursor: i64,
    events: Vec<ForwardJournalEventDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ForwardJournalEventDto {
    cursor: i64,
    event_type: String,
    entity_type: String,
    entity_id: Option<String>,
    signal_id: Option<String>,
    virtual_trade_id: Option<String>,
    symbol: Option<String>,
    plan: Option<String>,
    timeframe: Option<String>,
    direction: Option<String>,
    action: String,
    label: String,
    price: Option<String>,
    current_r: Option<String>,
    reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignalDebugDto {
    rejected_candidates: Vec<RejectedPlanCandidateDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RejectedPlanCandidateDto {
    candidate_id: Uuid,
    plan: String,
    timeframe: String,
    reason: String,
    payload: Value,
    evaluated_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LowPerformerAuditDto {
    rows: Vec<LowPerformerRowDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingAuditComparisonDto {
    symbol: String,
    current_snapshot_id: Option<Uuid>,
    previous_snapshot_id: Option<Uuid>,
    current_created_at_ms: Option<i64>,
    previous_created_at_ms: Option<i64>,
    rows: Vec<ScalpingAuditComparisonRowDto>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScalpingAuditComparisonRowDto {
    plan: String,
    timeframe: String,
    status: String,
    previous_status: Option<String>,
    status_changed: bool,
    closed_trades: usize,
    previous_closed_trades: usize,
    closed_trades_delta: isize,
    wins: usize,
    previous_wins: usize,
    wins_delta: isize,
    losses: usize,
    previous_losses: usize,
    losses_delta: isize,
    win_rate: f64,
    previous_win_rate: f64,
    win_rate_delta: f64,
    net_r: f64,
    previous_net_r: f64,
    net_r_delta: f64,
    expectancy_r: f64,
    previous_expectancy_r: f64,
    expectancy_r_delta: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LowPerformerRowDto {
    plan: String,
    timeframe: String,
    closed_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    net_r: f64,
    expectancy_r: f64,
    status: &'static str,
    reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeDecisionDto {
    id: String,
    entity_type: String,
    action: String,
    label: String,
    urgency: String,
    symbol: Option<String>,
    plan: Option<String>,
    timeframe: Option<String>,
    direction: Option<String>,
    signal_id: Option<String>,
    virtual_trade_id: Option<String>,
    entry_low: Option<String>,
    entry_high: Option<String>,
    projected_entry: Option<String>,
    actual_entry_price: Option<String>,
    primary_target: Option<String>,
    analytical_target: Option<String>,
    position_stage: Option<String>,
    remaining_fraction: Option<String>,
    realized_fraction: Option<String>,
    realized_r: Option<String>,
    trade_health: Option<String>,
    thesis_invalidation: Option<String>,
    hard_stop: Option<String>,
    active_stop: Option<String>,
    stop_loss: Option<String>,
    tp1_price: Option<String>,
    tp1_fraction: Option<String>,
    tp1_filled: Option<bool>,
    tp1_filled_at: Option<i64>,
    tp2_zone: Option<Value>,
    tp2_execution_edge: Option<String>,
    ambiguity: Option<String>,
    reconciliation_source: Option<String>,
    current_r: Option<String>,
    reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStrategyRowDto {
    plan: String,
    timeframe: String,
    interval: String,
    enabled: bool,
    release_id: Uuid,
    algorithm_version: String,
    parameter_version: String,
    tuning_status: String,
    candidate_label: Option<String>,
    tuning_source: String,
    required_timeframes: Vec<String>,
    minimum_history_bars: usize,
    warmup_bars: usize,
    parameters: Value,
    readiness: String,
    setup_state: String,
    trade_state: String,
    direction: String,
    tracking_health: String,
    active_signal_id: Value,
    active_virtual_trade_id: Value,
    evaluated_closed_candle_time: Option<i64>,
}

async fn strategy_summary(
    State(state): State<AppState>,
    Query(query): Query<StrategySummaryQuery>,
) -> Result<Json<RuntimeStrategySummaryDto>, RuntimeApiError> {
    let symbol =
        Symbol::new(query.symbol).map_err(|error| RuntimeApiError::invalid(error.to_string()))?;
    let configurations = state
        .configurations
        .ok_or_else(|| RuntimeApiError::unavailable("configuration store is not configured"))?;
    let monitoring = state
        .monitoring
        .ok_or_else(|| RuntimeApiError::unavailable("monitoring store is not configured"))?;

    let profiles = configurations
        .active_profiles(&symbol)
        .await
        .map_err(map_analysis_error)?;
    let snapshot = monitoring.snapshot().await.map_err(map_execution_error)?;
    let decisions = runtime_decisions(&snapshot.recommendations, &snapshot.virtual_trades);
    let active_overlay_count = snapshot.recommendations.len() + snapshot.virtual_trades.len();
    let open_trade_count = snapshot.virtual_trades.len();
    let summaries = snapshot_summary_map(snapshot.summaries);
    let rows = profiles
        .iter()
        .map(|profile| runtime_row(profile, summaries.get(&summary_key(profile))))
        .collect::<Result<Vec<_>, _>>()?;
    let locked_profiles = rows
        .iter()
        .filter(|row| row.tuning_status == "LOCKED")
        .count();
    Ok(Json(RuntimeStrategySummaryDto {
        symbol: symbol.as_str().to_owned(),
        release_id: rows.first().map(|row| row.release_id),
        total_profiles: rows.len(),
        locked_profiles,
        active_overlay_count,
        open_trade_count,
        decisions,
        rows,
    }))
}

async fn scalping_dashboard(
    State(state): State<AppState>,
    Query(query): Query<ScalpingDashboardQuery>,
) -> Result<Json<ScalpingDashboardDto>, RuntimeApiError> {
    build_scalping_dashboard(
        &state,
        &query.symbol,
        query.journal_limit,
        query.trade_limit,
    )
    .await
    .map(Json)
}

async fn create_scalping_audit_snapshot(
    State(state): State<AppState>,
    Query(query): Query<ScalpingDashboardQuery>,
) -> Result<Json<ScalpingAuditSnapshot>, RuntimeApiError> {
    let audits = state
        .scalping_audits
        .clone()
        .ok_or_else(|| RuntimeApiError::unavailable("scalping audit store is not configured"))?;
    let dashboard = build_scalping_dashboard(
        &state,
        &query.symbol,
        query.journal_limit,
        query.trade_limit,
    )
    .await?;
    let snapshot = audits
        .create(dashboard_to_snapshot(dashboard)?)
        .await
        .map_err(map_execution_error)?;
    Ok(Json(snapshot))
}

async fn list_scalping_audit_snapshots(
    State(state): State<AppState>,
    Query(query): Query<ScalpingAuditSnapshotListQuery>,
) -> Result<Json<ScalpingAuditSnapshotPage>, RuntimeApiError> {
    let limit = bounded_limit(query.limit.unwrap_or(20), 1, 100, "snapshot")?;
    let symbol =
        Symbol::new(query.symbol).map_err(|error| RuntimeApiError::invalid(error.to_string()))?;
    let audits = state
        .scalping_audits
        .ok_or_else(|| RuntimeApiError::unavailable("scalping audit store is not configured"))?;
    audits
        .list(ScalpingAuditSnapshotQuery {
            symbol: Some(symbol.as_str().to_owned()),
            cursor: query.cursor,
            limit,
        })
        .await
        .map(Json)
        .map_err(map_execution_error)
}

async fn scalping_audit_comparison(
    State(state): State<AppState>,
    Query(query): Query<ScalpingAuditSnapshotListQuery>,
) -> Result<Json<ScalpingAuditComparisonDto>, RuntimeApiError> {
    let symbol =
        Symbol::new(query.symbol).map_err(|error| RuntimeApiError::invalid(error.to_string()))?;
    let audits = state
        .scalping_audits
        .ok_or_else(|| RuntimeApiError::unavailable("scalping audit store is not configured"))?;
    let page = audits
        .list(ScalpingAuditSnapshotQuery {
            symbol: Some(symbol.as_str().to_owned()),
            cursor: None,
            limit: 2,
        })
        .await
        .map_err(map_execution_error)?;
    Ok(Json(snapshot_comparison(symbol.as_str(), &page.items)))
}

async fn build_scalping_dashboard(
    state: &AppState,
    symbol: &str,
    journal_limit: Option<u32>,
    trade_limit: Option<u32>,
) -> Result<ScalpingDashboardDto, RuntimeApiError> {
    let journal_limit = bounded_limit(journal_limit.unwrap_or(100), 1, 500, "journal")?;
    let trade_limit = bounded_limit(trade_limit.unwrap_or(200), 1, 500, "trade")?;
    let symbol =
        Symbol::new(symbol).map_err(|error| RuntimeApiError::invalid(error.to_string()))?;
    let configurations = state
        .configurations
        .clone()
        .ok_or_else(|| RuntimeApiError::unavailable("configuration store is not configured"))?;
    let monitoring = state
        .monitoring
        .clone()
        .ok_or_else(|| RuntimeApiError::unavailable("monitoring store is not configured"))?;
    let virtual_trades = state
        .virtual_trades
        .clone()
        .ok_or_else(|| RuntimeApiError::unavailable("virtual trade store is not configured"))?;

    let profiles = configurations
        .active_profiles(&symbol)
        .await
        .map_err(map_analysis_error)?;
    let snapshot = monitoring.snapshot().await.map_err(map_execution_error)?;
    let summaries = snapshot_summary_map(snapshot.summaries);
    let rows = profiles
        .iter()
        .filter(|profile| is_scalping_timeframe(profile.key.target_timeframe.as_str()))
        .map(|profile| runtime_row(profile, summaries.get(&summary_key(profile))))
        .collect::<Result<Vec<_>, _>>()?;
    let locked_profiles = rows
        .iter()
        .filter(|row| row.tuning_status == "LOCKED")
        .count();
    let events_after = snapshot
        .latest_cursor
        .saturating_sub(i64::from(journal_limit))
        .max(0);
    let journal_events = monitoring
        .events_after(events_after, journal_limit)
        .await
        .map_err(map_execution_error)?
        .events
        .into_iter()
        .filter(is_scalping_event)
        .map(forward_journal_event)
        .collect::<Vec<_>>();
    let mut audit = LowPerformerAudit::default();
    for timeframe in SCALPING_TIMEFRAMES {
        let page = virtual_trades
            .history(VirtualTradeHistoryQuery {
                symbol: Some(symbol.as_str().to_owned()),
                plan: None,
                timeframe: Some(timeframe.to_owned()),
                direction: None,
                exit_reason: None,
                from: None,
                to: None,
                cursor: None,
                limit: trade_limit,
            })
            .await
            .map_err(map_execution_error)?;
        audit.ingest_page(timeframe, page.items);
    }

    let active_overlay_count = snapshot
        .recommendations
        .iter()
        .filter(|value| value_timeframe(value).is_some_and(is_scalping_timeframe))
        .count()
        + snapshot
            .virtual_trades
            .iter()
            .filter(|value| value_timeframe(value).is_some_and(is_scalping_timeframe))
            .count();
    let open_trade_count = snapshot
        .virtual_trades
        .iter()
        .filter(|value| value_timeframe(value).is_some_and(is_scalping_timeframe))
        .count();
    let debug = signal_debug(state, &symbol).await?;

    Ok(ScalpingDashboardDto {
        symbol: symbol.as_str().to_owned(),
        mode: "SCALPING",
        timeframes: SCALPING_TIMEFRAMES.to_vec(),
        report: ScalpingReportDto {
            total_profiles: rows.len(),
            locked_profiles,
            baseline_profiles: rows.len().saturating_sub(locked_profiles),
            active_overlay_count,
            open_trade_count,
            rows,
        },
        journal: ForwardJournalDto {
            latest_cursor: snapshot.latest_cursor,
            events: journal_events,
        },
        debug,
        audit: LowPerformerAuditDto { rows: audit.rows() },
    })
}

async fn signal_debug(
    state: &AppState,
    symbol: &Symbol,
) -> Result<SignalDebugDto, RuntimeApiError> {
    let rejected_candidates = match state.rejected_candidates.clone() {
        Some(store) => store
            .recent_rejections(RejectedPlanCandidateQuery {
                symbol: symbol.clone(),
                timeframes: scalping_timeframes()?,
                limit: REJECTED_CANDIDATE_DEBUG_LIMIT,
            })
            .await
            .map_err(map_analysis_error)?
            .into_iter()
            .map(rejected_candidate_dto)
            .collect(),
        None => Vec::new(),
    };
    Ok(SignalDebugDto {
        rejected_candidates,
    })
}

fn scalping_timeframes() -> Result<Vec<Timeframe>, RuntimeApiError> {
    SCALPING_TIMEFRAMES
        .iter()
        .map(|value| {
            Timeframe::from_str(value)
                .map_err(|error| RuntimeApiError::unavailable(error.to_string()))
        })
        .collect()
}

fn rejected_candidate_dto(candidate: RejectedPlanCandidate) -> RejectedPlanCandidateDto {
    RejectedPlanCandidateDto {
        candidate_id: candidate.candidate_id,
        plan: candidate.key.plan.as_str().to_owned(),
        timeframe: candidate.key.target_timeframe.as_str().to_owned(),
        reason: candidate.reason,
        payload: candidate.payload,
        evaluated_at: candidate.evaluated_at,
    }
}

fn runtime_decisions(
    recommendations: &[Value],
    virtual_trades: &[Value],
) -> Vec<RuntimeDecisionDto> {
    recommendations
        .iter()
        .filter_map(recommendation_decision)
        .chain(virtual_trades.iter().filter_map(trade_decision))
        .collect()
}

fn recommendation_decision(value: &Value) -> Option<RuntimeDecisionDto> {
    let signal_id = string_field(Some(value), "signalId")?.to_owned();
    Some(RuntimeDecisionDto {
        id: signal_id.clone(),
        entity_type: "recommendation".to_owned(),
        action: "ENTRY".to_owned(),
        label: "entry".to_owned(),
        urgency: "ACTION".to_owned(),
        symbol: owned_field(value, "symbol"),
        plan: owned_field(value, "plan"),
        timeframe: owned_field(value, "timeframe").or_else(|| owned_field(value, "interval")),
        direction: owned_field(value, "direction"),
        signal_id: Some(signal_id),
        virtual_trade_id: None,
        entry_low: owned_field(value, "entryLow"),
        entry_high: owned_field(value, "entryHigh"),
        projected_entry: owned_field(value, "projectedEntry"),
        actual_entry_price: None,
        primary_target: owned_field(value, "primaryTarget"),
        analytical_target: owned_field(value, "analyticalTarget")
            .or_else(|| owned_field(value, "primaryTarget")),
        position_stage: owned_field(value, "positionStage"),
        remaining_fraction: owned_field(value, "remainingFraction"),
        realized_fraction: owned_field(value, "realizedFraction"),
        realized_r: owned_field(value, "realizedR"),
        trade_health: owned_field(value, "tradeHealth"),
        thesis_invalidation: owned_field(value, "thesisInvalidation"),
        hard_stop: owned_field(value, "hardStop"),
        active_stop: None,
        stop_loss: owned_field(value, "stopLoss"),
        tp1_price: owned_field(value, "tp1Price"),
        tp1_fraction: owned_field(value, "tp1Fraction"),
        tp1_filled: bool_field(value, "tp1Filled"),
        tp1_filled_at: i64_field(value, "tp1FilledAt"),
        tp2_zone: owned_value(value, "tp2Zone"),
        tp2_execution_edge: owned_field(value, "tp2ExecutionEdge"),
        ambiguity: owned_field(value, "ambiguity"),
        reconciliation_source: owned_field(value, "reconciliationSource"),
        current_r: owned_field(value, "currentR"),
        reason: Some("entry area active".to_owned()),
    })
}

fn trade_decision(value: &Value) -> Option<RuntimeDecisionDto> {
    let virtual_trade_id = string_field(Some(value), "virtualTradeId")?.to_owned();
    let (action, label, urgency) = trade_action(value);
    Some(RuntimeDecisionDto {
        id: virtual_trade_id.clone(),
        entity_type: "virtual_trade".to_owned(),
        action: action.to_owned(),
        label: label.to_owned(),
        urgency: urgency.to_owned(),
        symbol: owned_field(value, "symbol"),
        plan: owned_field(value, "plan"),
        timeframe: owned_field(value, "timeframe").or_else(|| owned_field(value, "interval")),
        direction: owned_field(value, "direction"),
        signal_id: owned_field(value, "signalId"),
        virtual_trade_id: Some(virtual_trade_id),
        entry_low: None,
        entry_high: None,
        projected_entry: None,
        actual_entry_price: owned_field(value, "actualEntryPrice"),
        primary_target: owned_field(value, "primaryTarget"),
        analytical_target: owned_field(value, "analyticalTarget"),
        position_stage: owned_field(value, "positionStage"),
        remaining_fraction: owned_field(value, "remainingFraction"),
        realized_fraction: owned_field(value, "realizedFraction"),
        realized_r: owned_field(value, "realizedR"),
        trade_health: owned_field(value, "tradeHealth"),
        thesis_invalidation: owned_field(value, "thesisInvalidation"),
        hard_stop: owned_field(value, "hardStop"),
        active_stop: owned_field(value, "activeStop"),
        stop_loss: owned_field(value, "initialStop"),
        tp1_price: owned_field(value, "tp1Price"),
        tp1_fraction: owned_field(value, "tp1Fraction"),
        tp1_filled: bool_field(value, "tp1Filled"),
        tp1_filled_at: i64_field(value, "tp1FilledAt"),
        tp2_zone: owned_value(value, "tp2Zone"),
        tp2_execution_edge: owned_field(value, "tp2ExecutionEdge"),
        ambiguity: owned_field(value, "ambiguity"),
        reconciliation_source: owned_field(value, "reconciliationSource"),
        current_r: owned_field(value, "currentR").or_else(|| owned_field(value, "grossR")),
        reason: owned_field(value, "exitReason").or_else(|| Some("manage open trade".to_owned())),
    })
}

fn trade_action(value: &Value) -> (&'static str, &'static str, &'static str) {
    match string_field(Some(value), "managementAction") {
        Some("EMERGENCY_EXIT") => ("EMERGENCY_EXIT", "emergency exit", "EXIT"),
        Some("SUGGEST_CLOSE") => ("SUGGEST_CLOSE", "close", "EXIT"),
        Some("MOVE_STOP") => ("MOVE_STOP", "move sl", "MANAGE"),
        Some("MOVE_TARGET") => ("MOVE_TARGET", "move tp", "MANAGE"),
        _ => ("MANAGE", "manage", "MANAGE"),
    }
}

fn runtime_row(
    profile: &MaterializedProfile,
    summary: Option<&Value>,
) -> Result<RuntimeStrategyRowDto, RuntimeApiError> {
    let tuning =
        runtime_profiles::tuning_metadata(&profile.key, &profile.versions.parameter_version);
    let plan = profile.key.plan.as_str().to_owned();
    let timeframe = profile.key.target_timeframe.as_str().to_owned();
    Ok(RuntimeStrategyRowDto {
        plan,
        timeframe: timeframe.clone(),
        interval: timeframe,
        enabled: profile.enabled,
        release_id: profile.versions.release_id,
        algorithm_version: profile.versions.algorithm_version.clone(),
        parameter_version: profile.versions.parameter_version.clone(),
        tuning_status: tuning.status.as_str().to_owned(),
        candidate_label: tuning.candidate_label.map(str::to_owned),
        tuning_source: tuning.source.to_owned(),
        required_timeframes: profile
            .required_timeframes
            .iter()
            .map(|timeframe| timeframe.as_str().to_owned())
            .collect(),
        minimum_history_bars: profile.minimum_history_bars,
        warmup_bars: profile.warmup_bars,
        parameters: serde_json::to_value(&profile.parameters)
            .map_err(|error| RuntimeApiError::unavailable(error.to_string()))?,
        readiness: string_field(summary, "readiness")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        setup_state: string_field(summary, "setupState")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        trade_state: string_field(summary, "tradeState")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        direction: string_field(summary, "direction")
            .unwrap_or("NONE")
            .to_owned(),
        tracking_health: string_field(summary, "trackingHealth")
            .unwrap_or("UNKNOWN")
            .to_owned(),
        active_signal_id: value_field(summary, "activeSignalId"),
        active_virtual_trade_id: value_field(summary, "activeVirtualTradeId"),
        evaluated_closed_candle_time: summary
            .and_then(|summary| summary.get("evaluatedClosedCandleTime"))
            .and_then(Value::as_i64),
    })
}

fn snapshot_summary_map(summaries: Vec<Value>) -> BTreeMap<(String, String), Value> {
    summaries
        .into_iter()
        .filter_map(|summary| {
            let plan = string_field(Some(&summary), "plan")?.to_owned();
            let timeframe = string_field(Some(&summary), "timeframe")
                .or_else(|| string_field(Some(&summary), "interval"))?
                .to_owned();
            Some(((plan, timeframe), summary))
        })
        .collect()
}

fn summary_key(profile: &MaterializedProfile) -> (String, String) {
    (
        profile.key.plan.as_str().to_owned(),
        profile.key.target_timeframe.as_str().to_owned(),
    )
}

fn string_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
}

fn owned_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn i64_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn owned_value(value: &Value, key: &str) -> Option<Value> {
    value.get(key).filter(|value| !value.is_null()).cloned()
}

fn value_field(value: Option<&Value>, key: &str) -> Value {
    value
        .and_then(|value| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn dashboard_to_snapshot(
    dashboard: ScalpingDashboardDto,
) -> Result<NewScalpingAuditSnapshot, RuntimeApiError> {
    Ok(NewScalpingAuditSnapshot {
        symbol: dashboard.symbol,
        mode: dashboard.mode.to_owned(),
        timeframes: dashboard
            .timeframes
            .into_iter()
            .map(str::to_owned)
            .collect(),
        report: serde_json::to_value(dashboard.report)
            .map_err(|error| RuntimeApiError::unavailable(error.to_string()))?,
        journal: serde_json::to_value(dashboard.journal)
            .map_err(|error| RuntimeApiError::unavailable(error.to_string()))?,
        audit: serde_json::to_value(dashboard.audit)
            .map_err(|error| RuntimeApiError::unavailable(error.to_string()))?,
    })
}

fn snapshot_comparison(
    symbol: &str,
    snapshots: &[ScalpingAuditSnapshot],
) -> ScalpingAuditComparisonDto {
    let current = snapshots.first();
    let previous = snapshots.get(1);
    let rows = match current {
        Some(current) => compare_audit_rows(current, previous),
        None => Vec::new(),
    };
    ScalpingAuditComparisonDto {
        symbol: symbol.to_owned(),
        current_snapshot_id: current.map(|snapshot| snapshot.snapshot_id),
        previous_snapshot_id: previous.map(|snapshot| snapshot.snapshot_id),
        current_created_at_ms: current.map(|snapshot| snapshot.created_at_ms),
        previous_created_at_ms: previous.map(|snapshot| snapshot.created_at_ms),
        rows,
    }
}

fn compare_audit_rows(
    current: &ScalpingAuditSnapshot,
    previous: Option<&ScalpingAuditSnapshot>,
) -> Vec<ScalpingAuditComparisonRowDto> {
    let current_rows = audit_metric_rows(current);
    let previous_rows = previous.map(audit_metric_rows).unwrap_or_default();
    let keys = current_rows
        .keys()
        .chain(previous_rows.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut rows = keys
        .into_iter()
        .filter_map(|key| {
            let current = current_rows.get(&key);
            let previous = previous_rows.get(&key);
            comparison_row(&key, current, previous)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        scalping_order(&left.timeframe)
            .cmp(&scalping_order(&right.timeframe))
            .then(left.plan.cmp(&right.plan))
    });
    rows
}

fn comparison_row(
    key: &(String, String),
    current: Option<&AuditMetricRow>,
    previous: Option<&AuditMetricRow>,
) -> Option<ScalpingAuditComparisonRowDto> {
    let row = current.or(previous)?;
    let previous_status = previous.map(|row| row.status.clone());
    let previous_closed_trades = previous.map(|row| row.closed_trades).unwrap_or_default();
    let previous_wins = previous.map(|row| row.wins).unwrap_or_default();
    let previous_losses = previous.map(|row| row.losses).unwrap_or_default();
    let previous_win_rate = previous.map(|row| row.win_rate).unwrap_or_default();
    let previous_net_r = previous.map(|row| row.net_r).unwrap_or_default();
    let previous_expectancy_r = previous.map(|row| row.expectancy_r).unwrap_or_default();
    let closed_trades = current.map(|row| row.closed_trades).unwrap_or_default();
    let wins = current.map(|row| row.wins).unwrap_or_default();
    let losses = current.map(|row| row.losses).unwrap_or_default();
    let win_rate = current.map(|row| row.win_rate).unwrap_or_default();
    let net_r = current.map(|row| row.net_r).unwrap_or_default();
    let expectancy_r = current.map(|row| row.expectancy_r).unwrap_or_default();
    let status = current
        .map(|row| row.status.clone())
        .unwrap_or_else(|| "MISSING".to_owned());
    Some(ScalpingAuditComparisonRowDto {
        plan: row.plan.clone().unwrap_or_else(|| key.1.clone()),
        timeframe: row.timeframe.clone().unwrap_or_else(|| key.0.clone()),
        status: status.clone(),
        previous_status: previous_status.clone(),
        status_changed: previous_status.is_some_and(|previous| previous != status),
        closed_trades,
        previous_closed_trades,
        closed_trades_delta: closed_trades as isize - previous_closed_trades as isize,
        wins,
        previous_wins,
        wins_delta: wins as isize - previous_wins as isize,
        losses,
        previous_losses,
        losses_delta: losses as isize - previous_losses as isize,
        win_rate,
        previous_win_rate,
        win_rate_delta: win_rate - previous_win_rate,
        net_r,
        previous_net_r,
        net_r_delta: net_r - previous_net_r,
        expectancy_r,
        previous_expectancy_r,
        expectancy_r_delta: expectancy_r - previous_expectancy_r,
    })
}

#[derive(Clone, Debug, Default)]
struct AuditMetricRow {
    plan: Option<String>,
    timeframe: Option<String>,
    status: String,
    closed_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    net_r: f64,
    expectancy_r: f64,
}

fn audit_metric_rows(
    snapshot: &ScalpingAuditSnapshot,
) -> BTreeMap<(String, String), AuditMetricRow> {
    snapshot
        .audit
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let timeframe = owned_field(row, "timeframe")?;
            let plan = owned_field(row, "plan")?;
            Some((
                (timeframe.clone(), plan.clone()),
                AuditMetricRow {
                    plan: Some(plan),
                    timeframe: Some(timeframe),
                    status: owned_field(row, "status").unwrap_or_else(|| "UNKNOWN".to_owned()),
                    closed_trades: usize_field(row, "closedTrades"),
                    wins: usize_field(row, "wins"),
                    losses: usize_field(row, "losses"),
                    win_rate: decimal_field(row, "winRate").unwrap_or_default(),
                    net_r: decimal_field(row, "netR").unwrap_or_default(),
                    expectancy_r: decimal_field(row, "expectancyR").unwrap_or_default(),
                },
            ))
        })
        .collect()
}

fn usize_field(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64().and_then(|number| number.try_into().ok()),
            Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or_default()
}

fn bounded_limit(
    limit: u32,
    min: u32,
    max: u32,
    label: &'static str,
) -> Result<u32, RuntimeApiError> {
    if limit < min || limit > max {
        return Err(RuntimeApiError::invalid(format!(
            "{label} limit must be {min}..={max}"
        )));
    }
    Ok(limit)
}

fn is_scalping_timeframe(timeframe: &str) -> bool {
    SCALPING_TIMEFRAMES.contains(&timeframe)
}

fn value_timeframe(value: &Value) -> Option<&str> {
    string_field(Some(value), "timeframe").or_else(|| string_field(Some(value), "interval"))
}

fn is_scalping_event(event: &MonitoringEventEnvelope) -> bool {
    value_timeframe(&event.payload).is_some_and(is_scalping_timeframe)
}

fn forward_journal_event(event: MonitoringEventEnvelope) -> ForwardJournalEventDto {
    let action = journal_action(&event.event_type);
    ForwardJournalEventDto {
        cursor: event.cursor,
        event_type: event.event_type,
        entity_type: event.entity_type,
        entity_id: event.entity_id.map(|id| id.to_string()),
        signal_id: owned_field(&event.payload, "signalId"),
        virtual_trade_id: owned_field(&event.payload, "virtualTradeId"),
        symbol: owned_field(&event.payload, "symbol"),
        plan: owned_field(&event.payload, "plan"),
        timeframe: value_timeframe(&event.payload).map(str::to_owned),
        direction: owned_field(&event.payload, "direction"),
        action: action.0.to_owned(),
        label: action.1.to_owned(),
        price: owned_field(&event.payload, "price")
            .or_else(|| owned_field(&event.payload, "actualEntryPrice"))
            .or_else(|| owned_field(&event.payload, "exitPrice")),
        current_r: owned_field(&event.payload, "currentR")
            .or_else(|| owned_field(&event.payload, "grossR")),
        reason: owned_field(&event.payload, "reason")
            .or_else(|| owned_field(&event.payload, "exitReason")),
    }
}

fn journal_action(event_type: &str) -> (&'static str, &'static str) {
    match event_type {
        "recommendation-opened" => ("ENTRY", "entry setup"),
        "recommendation-expired" => ("EXPIRED", "expired"),
        "recommendation-invalidated" => ("INVALIDATED", "invalidated"),
        "virtual-trade-opened" => ("ENTERED", "entered"),
        "virtual-trade-stop-moved" => ("MOVE_STOP", "move sl"),
        "virtual-trade-target-moved" => ("MOVE_TARGET", "move tp"),
        "virtual-trade-closed" => ("CLOSED", "closed"),
        "virtual-trade-reversed" => ("REVERSED", "reversed"),
        _ => ("INFO", "event"),
    }
}

#[derive(Default)]
struct LowPerformerAudit {
    groups: BTreeMap<(String, String), LowPerformerAccumulator>,
}

impl LowPerformerAudit {
    fn ingest_page(&mut self, fallback_timeframe: &str, trades: Vec<Value>) {
        for trade in trades {
            if !is_closed_trade(&trade) {
                continue;
            }
            let plan = owned_field(&trade, "plan").unwrap_or_else(|| "UNKNOWN".to_owned());
            let timeframe = value_timeframe(&trade)
                .unwrap_or(fallback_timeframe)
                .to_owned();
            let gross_r = decimal_field(&trade, "grossR").unwrap_or(0.0);
            self.groups
                .entry((timeframe, plan))
                .or_default()
                .ingest(gross_r);
        }
    }

    fn rows(self) -> Vec<LowPerformerRowDto> {
        let mut rows = self
            .groups
            .into_iter()
            .map(|((timeframe, plan), accumulator)| accumulator.into_row(plan, timeframe))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            scalping_order(&left.timeframe)
                .cmp(&scalping_order(&right.timeframe))
                .then(left.plan.cmp(&right.plan))
        });
        rows
    }
}

#[derive(Default)]
struct LowPerformerAccumulator {
    closed_trades: usize,
    wins: usize,
    losses: usize,
    net_r: f64,
}

impl LowPerformerAccumulator {
    fn ingest(&mut self, gross_r: f64) {
        self.closed_trades += 1;
        self.net_r += gross_r;
        if gross_r > 0.0 {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
    }

    fn into_row(self, plan: String, timeframe: String) -> LowPerformerRowDto {
        let win_rate = if self.closed_trades == 0 {
            0.0
        } else {
            self.wins as f64 / self.closed_trades as f64
        };
        let expectancy_r = if self.closed_trades == 0 {
            0.0
        } else {
            self.net_r / self.closed_trades as f64
        };
        let (status, reason) = if self.closed_trades < 10 {
            ("WATCH", "sample below 10 closed trades".to_owned())
        } else if expectancy_r < 0.0 {
            (
                "LOW_PERFORMER",
                "negative expectancy on recent live trades".to_owned(),
            )
        } else if win_rate < 0.35 {
            (
                "LOW_PERFORMER",
                "win rate below 35% on recent live trades".to_owned(),
            )
        } else {
            ("OK", "recent live sample is acceptable".to_owned())
        };
        LowPerformerRowDto {
            plan,
            timeframe,
            closed_trades: self.closed_trades,
            wins: self.wins,
            losses: self.losses,
            win_rate,
            net_r: self.net_r,
            expectancy_r,
            status,
            reason,
        }
    }
}

fn is_closed_trade(value: &Value) -> bool {
    string_field(Some(value), "tradeState")
        .or_else(|| string_field(Some(value), "trade_state"))
        .is_some_and(|state| state == "CLOSED")
        || !value.get("exitTime").unwrap_or(&Value::Null).is_null()
}

fn decimal_field(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(|value| match value {
        Value::String(text) => text.parse::<f64>().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    })
}

fn scalping_order(timeframe: &str) -> usize {
    SCALPING_TIMEFRAMES
        .iter()
        .position(|candidate| *candidate == timeframe)
        .unwrap_or(usize::MAX)
}

fn map_analysis_error(error: AnalysisServiceError) -> RuntimeApiError {
    RuntimeApiError::unavailable(error.to_string())
}

fn map_execution_error(error: ExecutionError) -> RuntimeApiError {
    RuntimeApiError::unavailable(error.to_string())
}
