use crate::{
    dto::{
        ApiErrorDto, CandleDto, CandlePageDto, MarketDto, ResyncDto, SnapshotDto, StatusDto,
        SyncStatusDto,
    },
    runtime::build_facts,
    state::AppState,
    support_resistance::{SupportResistanceZoneDto, active_zones_from_facts},
};
use analysis_core::calculate_rsi;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures_util::{Stream, StreamExt, stream};
use market_domain::{MarketEvent, Symbol, Timeframe};
use market_service::ServiceError;
use runtime_observability::record_subscriber_lag;
use serde::Deserialize;
use serde::Serialize;
use std::{convert::Infallible, str::FromStr, time::Duration};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

const MAX_HISTORY_LIMIT: u16 = 5_000;
const DEFAULT_SUPPORT_RESISTANCE_LOOKBACK: u16 = 2_000;
const MIN_SUPPORT_RESISTANCE_LOOKBACK: u16 = 100;

pub fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/api/v1/markets", get(markets))
        .route("/api/v1/candles", get(candles))
        .route("/api/v1/market-stream", get(market_stream))
        .route("/api/v1/market-metrics/rsi", get(rsi_metrics))
        .route("/api/v1/sync-status", get(sync_status))
        .route("/api/v1/support-resistance", get(support_resistance))
        .merge(crate::analysis_api::router())
        .merge(crate::backtest_api::router())
        .merge(crate::health_api::router())
        .merge(crate::monitoring_api::router())
        .merge(crate::runtime_api::router())
        .merge(crate::virtual_trade_api::router())
        .with_state(app_state)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn from_service(error: ServiceError) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "storage_unavailable",
            message: error.to_string(),
        }
    }

    fn serialization(error: serde_json::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "serialization_error",
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
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
struct MarketQuery {
    symbol: String,
}

#[derive(Deserialize)]
struct CandleQuery {
    symbol: String,
    interval: String,
    before: Option<i64>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
struct SupportResistanceQuery {
    symbol: String,
    interval: String,
    limit: Option<usize>,
    lookback: Option<u16>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RsiMetricDto {
    timeframe: Timeframe,
    value: Option<String>,
}

#[derive(Deserialize)]
struct StreamQuery {
    symbol: String,
    interval: String,
}

async fn markets(State(state): State<AppState>) -> Result<Json<Vec<MarketDto>>, ApiError> {
    let markets = state
        .markets
        .enabled_markets()
        .await
        .map_err(ApiError::from_service)?;
    Ok(Json(markets.into_iter().map(Into::into).collect()))
}

async fn candles(
    State(state): State<AppState>,
    Query(query): Query<CandleQuery>,
) -> Result<Json<CandlePageDto>, ApiError> {
    let symbol = parse_symbol(&query.symbol)?;
    let timeframe = parse_timeframe(&query.interval)?;
    let limit = query.limit.unwrap_or(state.default_history_limit);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(ApiError::invalid("limit must be between 1 and 5000"));
    }

    let rows = state
        .candles
        .latest(&symbol, timeframe, query.before, i64::from(limit))
        .await
        .map_err(ApiError::from_service)?;
    let next_before = rows.first().map(|candle| candle.open_time);
    Ok(Json(CandlePageDto {
        data: rows.into_iter().map(Into::into).collect(),
        next_before,
    }))
}

async fn sync_status(
    State(state): State<AppState>,
    Query(query): Query<MarketQuery>,
) -> Result<Json<Vec<SyncStatusDto>>, ApiError> {
    let symbol = parse_symbol(&query.symbol)?;
    let statuses = state
        .sync
        .all_for_symbol(&symbol)
        .await
        .map_err(ApiError::from_service)?;
    Ok(Json(statuses.into_iter().map(Into::into).collect()))
}

async fn support_resistance(
    State(state): State<AppState>,
    Query(query): Query<SupportResistanceQuery>,
) -> Result<Json<Vec<SupportResistanceZoneDto>>, ApiError> {
    let symbol = parse_symbol(&query.symbol)?;
    let timeframe = parse_timeframe(&query.interval)?;
    let limit = query.limit.unwrap_or(20).min(100);
    let lookback = query
        .lookback
        .unwrap_or(DEFAULT_SUPPORT_RESISTANCE_LOOKBACK);
    if !(MIN_SUPPORT_RESISTANCE_LOOKBACK..=MAX_HISTORY_LIMIT).contains(&lookback) {
        return Err(ApiError::invalid("lookback must be between 100 and 5000"));
    }
    let candles = state
        .candles
        .latest(&symbol, timeframe, None, i64::from(lookback))
        .await
        .map_err(ApiError::from_service)?;
    if candles.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let facts = build_facts(&candles).map_err(|error| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "analysis_unavailable",
        message: error.to_string(),
    })?;
    Ok(Json(active_zones_from_facts(&facts, &candles, limit)))
}

async fn rsi_metrics(
    State(state): State<AppState>,
    Query(query): Query<MarketQuery>,
) -> Result<Json<Vec<RsiMetricDto>>, ApiError> {
    let symbol = parse_symbol(&query.symbol)?;
    let mut rows = Vec::with_capacity(Timeframe::ALL.len());
    for timeframe in Timeframe::ALL {
        let candles = state
            .candles
            .latest(&symbol, timeframe, None, 200)
            .await
            .map_err(ApiError::from_service)?;
        let closes = candles
            .iter()
            .map(|candle| candle.close)
            .collect::<Vec<_>>();
        let value = calculate_rsi(&closes, 14)
            .map_err(|error| ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "analysis_unavailable",
                message: error.to_string(),
            })?
            .values
            .into_iter()
            .flatten()
            .last()
            .map(|value| value.to_string());
        rows.push(RsiMetricDto { timeframe, value });
    }
    Ok(Json(rows))
}

async fn market_stream(
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let symbol = parse_symbol(&query.symbol)?;
    let timeframe = parse_timeframe(&query.interval)?;
    let receiver = state.hub.subscribe();
    let snapshot = state.hub.snapshot(&symbol, timeframe).await;
    let first = named_event("snapshot", SnapshotDto::from(snapshot))?;
    let requested_symbol = symbol.clone();

    let live = BroadcastStream::new(receiver).filter_map(move |message| {
        let event = match message {
            Ok(event) => map_market_event(event, &requested_symbol, timeframe),
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                record_subscriber_lag();
                named_event(
                    "resync",
                    ResyncDto {
                        reason: format!("subscriber lagged by {skipped} events"),
                    },
                )
                .ok()
            }
        };
        async move { event.map(Ok) }
    });
    let events = stream::once(async move { Ok(first) }).chain(live);

    Ok(Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

fn parse_symbol(value: &str) -> Result<Symbol, ApiError> {
    Symbol::new(value).map_err(|error| ApiError::invalid(error.to_string()))
}

fn parse_timeframe(value: &str) -> Result<Timeframe, ApiError> {
    Timeframe::from_str(value).map_err(|error| ApiError::invalid(error.to_string()))
}

fn named_event(name: &'static str, data: impl Serialize) -> Result<Event, ApiError> {
    let data = serde_json::to_string(&data).map_err(ApiError::serialization)?;
    Ok(Event::default().event(name).data(data))
}

fn map_market_event(event: MarketEvent, symbol: &Symbol, timeframe: Timeframe) -> Option<Event> {
    match event {
        MarketEvent::FormingCandle(candle)
            if &candle.symbol == symbol && candle.timeframe == timeframe =>
        {
            named_event("forming", CandleDto::from(candle)).ok()
        }
        MarketEvent::ClosedCandle(candle)
            if &candle.symbol == symbol && candle.timeframe == timeframe =>
        {
            named_event("closed", CandleDto::from(candle)).ok()
        }
        MarketEvent::StatusChanged {
            symbol: event_symbol,
            status,
        } if &event_symbol == symbol => named_event("status", StatusDto { status }).ok(),
        MarketEvent::ResyncRequired {
            symbol: event_symbol,
            reason,
        } if &event_symbol == symbol => named_event("resync", ResyncDto { reason }).ok(),
        _ => None,
    }
}
