use crate::{config::AppConfig, runtime::recovery::reconcile_active_from_m1};
use analysis_domain::{DirectiveKind, ManagementDirective};
use binance_adapter::BinanceRestClient;
use execution_service::{ExecutionSupervisor, SupervisorEvent, SupervisorStep};
use execution_storage::TrackerRepository;
use market_domain::{Symbol, Timeframe};
use market_storage::CandleRepository;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

pub const ROLLOVER_REASON: &str = "DEPLOYMENT_VERSION_ROLLOVER";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentRolloverArgs {
    pub symbol: Symbol,
    pub apply: bool,
}

pub fn parse_args(
    args: impl IntoIterator<Item = String>,
) -> Result<DeploymentRolloverArgs, String> {
    let mut args = args.into_iter();
    let mut symbol = Symbol::new("XAUUSDT").map_err(|error| error.to_string())?;
    let mut apply = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--symbol" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--symbol requires a value".to_owned())?;
                symbol = Symbol::new(value).map_err(|error| error.to_string())?;
            }
            "--apply" => apply = true,
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }
    Ok(DeploymentRolloverArgs { symbol, apply })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentRolloverReport {
    pub database: String,
    pub initial_open: i64,
    pub synced_m1_candles: usize,
    pub naturally_closed: i64,
    pub rolled_over: i64,
    pub remaining_open: i64,
    pub rollover_price: Decimal,
    pub rollover_time: i64,
    pub applied: bool,
}

#[derive(Clone, Debug)]
struct OpenTrade {
    virtual_trade_id: Uuid,
    signal_id: Uuid,
    algorithm_version: String,
    parameter_version: String,
}

pub async fn run(
    config: &AppConfig,
    symbol: &Symbol,
    apply: bool,
) -> Result<DeploymentRolloverReport, BoxError> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&config.database_url)
        .await?;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    let initial_open = open_trade_count(&pool, symbol).await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let source = BinanceRestClient::new(http, config.binance_rest_base_url.clone());
    let server_time = source.server_time().await?;
    let preview = source
        .klines(symbol, Timeframe::M1, None, Some(server_time), 2)
        .await?
        .into_iter()
        .rfind(|candle| candle.is_closed_at(server_time))
        .ok_or("Binance returned no closed 1m candle")?;

    if !apply {
        pool.close().await;
        return Ok(DeploymentRolloverReport {
            database,
            initial_open,
            synced_m1_candles: 0,
            naturally_closed: 0,
            rolled_over: 0,
            remaining_open: initial_open,
            rollover_price: preview.close,
            rollover_time: preview.close_time,
            applied: false,
        });
    }

    let candles = CandleRepository::new(pool.clone());
    let synced_m1_candles = catch_up_m1(
        &source,
        &candles,
        symbol,
        server_time,
        config.backfill_page_limit,
    )
    .await?;
    let latest = candles
        .latest(symbol, Timeframe::M1, None, 1)
        .await?
        .into_iter()
        .next()
        .ok_or("database has no closed 1m candle after catch-up")?;

    let tracker = Arc::new(TrackerRepository::new(pool.clone()));
    let mut supervisor = ExecutionSupervisor::new(tracker);
    supervisor.restore_active(symbol).await?;
    reconcile_active_from_m1(&mut supervisor, &pool, symbol).await?;

    let after_reconciliation = open_trade_count(&pool, symbol).await?;
    let naturally_closed = initial_open.saturating_sub(after_reconciliation);
    let remaining = open_trades(&pool, symbol).await?;
    for trade in remaining {
        let directive = ManagementDirective {
            directive_id: Uuid::now_v7(),
            signal_id: trade.signal_id,
            virtual_trade_id: Some(trade.virtual_trade_id),
            kind: DirectiveKind::SuggestClose,
            effective_at: latest.close_time,
            price: Some(latest.close),
            reason: ROLLOVER_REASON.to_owned(),
            source_closed_candle_time: latest.close_time,
            algorithm_version: trade.algorithm_version,
            parameter_version: trade.parameter_version,
            reverse_signal: None,
        };
        insert_directive(&pool, &directive).await?;
        let step = supervisor
            .process(SupervisorEvent::Directive(directive))
            .await?;
        if step != SupervisorStep::Committed {
            return Err(format!(
                "rollover for trade {} was not committed: {step:?}",
                trade.virtual_trade_id
            )
            .into());
        }
    }

    let remaining_open = open_trade_count(&pool, symbol).await?;
    let rolled_over = after_reconciliation.saturating_sub(remaining_open);
    pool.close().await;
    Ok(DeploymentRolloverReport {
        database,
        initial_open,
        synced_m1_candles,
        naturally_closed,
        rolled_over,
        remaining_open,
        rollover_price: latest.close,
        rollover_time: latest.close_time,
        applied: true,
    })
}

async fn catch_up_m1(
    source: &BinanceRestClient,
    candles: &CandleRepository,
    symbol: &Symbol,
    server_time: i64,
    page_limit: u16,
) -> Result<usize, BoxError> {
    let latest = candles
        .bounds(symbol, Timeframe::M1)
        .await?
        .map(|(_, latest)| latest);
    let mut start = latest.map(|open_time| open_time + Timeframe::M1.duration_ms());
    let mut synced = 0;
    loop {
        let fetched = source
            .klines(symbol, Timeframe::M1, start, Some(server_time), page_limit)
            .await?;
        let fetched_count = fetched.len();
        let closed = fetched
            .into_iter()
            .filter(|candle| candle.is_closed_at(server_time))
            .collect::<Vec<_>>();
        if closed.is_empty() {
            return Ok(synced);
        }
        let latest_open = closed.last().expect("closed page is non-empty").open_time;
        candles.upsert_page(&closed).await?;
        synced += closed.len();
        if fetched_count < usize::from(page_limit) {
            return Ok(synced);
        }
        start = Some(latest_open + Timeframe::M1.duration_ms());
    }
}

async fn open_trade_count(pool: &PgPool, symbol: &Symbol) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM virtual_trades WHERE symbol = $1 AND trade_state = 'OPEN'",
    )
    .bind(symbol.as_str())
    .fetch_one(pool)
    .await
}

async fn open_trades(pool: &PgPool, symbol: &Symbol) -> Result<Vec<OpenTrade>, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT virtual_trade_id, signal_id, algorithm_version, parameter_version \
         FROM virtual_trades WHERE symbol = $1 AND trade_state = 'OPEN' \
         ORDER BY actual_entry_time, virtual_trade_id",
    )
    .bind(symbol.as_str())
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| OpenTrade {
        virtual_trade_id: row.get("virtual_trade_id"),
        signal_id: row.get("signal_id"),
        algorithm_version: row.get("algorithm_version"),
        parameter_version: row.get("parameter_version"),
    })
    .collect())
}

async fn insert_directive(pool: &PgPool, directive: &ManagementDirective) -> Result<(), BoxError> {
    sqlx::query(
        "INSERT INTO trade_management_directives \
         (directive_id, signal_id, virtual_trade_id, kind, payload, effective_at, \
          source_closed_candle_time, algorithm_version, parameter_version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(directive.directive_id)
    .bind(directive.signal_id)
    .bind(directive.virtual_trade_id)
    .bind(directive.kind.as_str())
    .bind(serde_json::to_value(directive)?)
    .bind(directive.effective_at)
    .bind(directive.source_closed_candle_time)
    .bind(&directive.algorithm_version)
    .bind(&directive.parameter_version)
    .execute(pool)
    .await?;
    Ok(())
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;
