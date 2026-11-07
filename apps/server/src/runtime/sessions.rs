use crate::{
    config::AppConfig,
    session_history::{HistoricalSessionCoordinator, load_session_overrides},
};
use binance_adapter::BinanceRestClient;
use market_domain::Timeframe;
use market_service::ServiceError;
use market_storage::{CandleRepository, SessionRepository};
use std::sync::Arc;

pub async fn prepare_historical_sessions(
    config: &AppConfig,
    source: Arc<BinanceRestClient>,
    sessions: Arc<SessionRepository>,
    candles: Arc<CandleRepository>,
    market: &market_domain::Market,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let official = source.trading_schedule(&market.session_market).await?;
    sessions.upsert_sessions(&official).await?;
    let official_end = official
        .iter()
        .map(|session| session.end_time)
        .max()
        .ok_or_else(|| {
            ServiceError::InvalidData("official session schedule is empty".to_owned())
        })?;
    let source_event_time = official
        .iter()
        .map(|session| session.source_event_time)
        .max()
        .unwrap_or(official_end);
    let mut history_start = market.onboard_time;
    for timeframe in Timeframe::ALL {
        if let Some((oldest, _)) = candles.bounds(&market.symbol, timeframe).await? {
            history_start = history_start.min(oldest);
        }
    }
    let overrides = load_session_overrides(&config.session_history, &market.session_market)?;
    let report = HistoricalSessionCoordinator::new(
        (*sessions).clone(),
        config.session_history.clone(),
        overrides,
    )
    .ensure_coverage(
        &market.session_market,
        history_start,
        official_end,
        source_event_time,
        true,
    )
    .await?;
    tracing::info!(
        session_market = %market.session_market,
        start_time = report.start_time,
        end_time = report.end_time,
        regular_ms = report.regular_ms,
        no_trading_ms = report.no_trading_ms,
        policy_version = %report.policy_version,
        override_version = %report.override_version,
        "canonical market session coverage ready"
    );
    Ok(())
}
