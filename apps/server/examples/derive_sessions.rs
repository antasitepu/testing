//! Persists derived market sessions for a research database, using the same
//! generator, policy and overrides as the runtime's historical coverage.
//!
//! cargo run -p gold-monitoring-server --example derive_sessions -- \
//!   --database-url <url> --session-market COMMODITY --start <ms> --end <ms>
use gold_monitoring_server::session_history::{
    HistoricalSessionCoordinator, SessionHistoryConfig, load_session_overrides,
};
use market_storage::SessionRepository;
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;

/// Runtime defaults from `apps/server/src/config.rs`.
const OVERRIDES_PATH: &str = "config/commodity-session-overrides.json";
const POLICY_VERSION: &str = "commodity-session-policy-v2";

struct Args {
    database_url: String,
    session_market: String,
    start: i64,
    end: i64,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = args.into_iter();
    let (mut database_url, mut session_market, mut start, mut end) = (None, None, None, None);
    while let Some(argument) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--database-url" => database_url = Some(value()?),
            "--session-market" => session_market = Some(value()?),
            "--start" => start = Some(value()?.parse().map_err(|_| "--start must be ms")?),
            "--end" => end = Some(value()?.parse().map_err(|_| "--end must be ms")?),
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }
    Ok(Args {
        database_url: database_url.ok_or("--database-url is required")?,
        session_market: session_market.ok_or("--session-market is required")?,
        start: start.ok_or("--start is required")?,
        end: end.ok_or("--end is required")?,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = parse_args(std::env::args().skip(1))
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let config = SessionHistoryConfig {
        overrides_path: PathBuf::from(OVERRIDES_PATH),
        policy_version: POLICY_VERSION.to_owned(),
    };
    let overrides = load_session_overrides(&config, &args.session_market)?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    // Like the runtime without an official schedule: the event time is the end.
    let report =
        HistoricalSessionCoordinator::new(SessionRepository::new(pool.clone()), config, overrides)
            .ensure_coverage(&args.session_market, args.start, args.end, args.end, true)
            .await?;
    println!(
        "sessions {} [{}, {}): regular_ms={} no_trading_ms={} unknown_ms={} policy={} overrides={}",
        args.session_market,
        report.start_time,
        report.end_time,
        report.regular_ms,
        report.no_trading_ms,
        report.unknown_ms,
        report.policy_version,
        report.override_version
    );
    pool.close().await;
    Ok(())
}
