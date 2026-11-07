use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use url::Url;

use crate::session_history::SessionHistoryConfig;

const DEFAULT_BIND: &str = "127.0.0.1:3000";
const DEFAULT_REST_URL: &str = "https://fapi.binance.com";
const DEFAULT_WEBSOCKET_URL: &str = "wss://fstream.binance.com/market";
const DEFAULT_STATIC_DIR: &str = "web/dist";
const REQUIRED_INITIAL_HISTORY_LIMIT: u16 = 1_000;
const DEFAULT_BACKFILL_PAGE_LIMIT: u16 = 1_500;
const MAX_BACKFILL_PAGE_LIMIT: u16 = 1_500;
const DEFAULT_ANALYSIS_WORKER_ID: &str = "local-1";
const DEFAULT_ANALYSIS_LEASE_SECONDS: u16 = 30;
const DEFAULT_ANALYSIS_BARRIER_GRACE_SECONDS: u16 = 10;
const DEFAULT_BACKTEST_MAX_CONCURRENT_RUNS: u16 = 1;
const DEFAULT_SESSION_OVERRIDES_PATH: &str = "config/commodity-session-overrides.json";
const DEFAULT_SESSION_POLICY_VERSION: &str = "commodity-session-policy-v2";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    #[error("invalid environment variable {name}: {message}")]
    Invalid { name: &'static str, message: String },
    #[error("APP_BIND must use a loopback address")]
    NonLoopback,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub bind: SocketAddr,
    /// The one public host a reverse proxy may forward, besides loopback.
    pub public_host: Option<String>,
    pub database_url: String,
    pub binance_rest_base_url: Url,
    pub binance_ws_base_url: Url,
    pub static_dir: PathBuf,
    pub initial_history_limit: u16,
    pub backfill_page_limit: u16,
    pub analysis_worker_id: String,
    pub analysis_lease_seconds: u16,
    pub analysis_barrier_grace_seconds: u16,
    pub backtest_max_concurrent_runs: u16,
    pub session_history: SessionHistoryConfig,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();
        Self::from_values(&std::env::vars().collect())
    }

    pub fn from_values(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let database_url = required(values, "DATABASE_URL")?;
        let bind = parse_socket(values, "APP_BIND", DEFAULT_BIND)?;
        if !bind.ip().is_loopback() {
            return Err(ConfigError::NonLoopback);
        }
        let public_host = parse_public_host(values)?;

        let binance_rest_base_url = parse_url(
            values,
            "BINANCE_REST_BASE_URL",
            DEFAULT_REST_URL,
            &["http", "https"],
        )?;
        let binance_ws_base_url = parse_url(
            values,
            "BINANCE_WS_BASE_URL",
            DEFAULT_WEBSOCKET_URL,
            &["ws", "wss"],
        )?;
        let initial_history_limit = parse_u16(
            values,
            "INITIAL_HISTORY_LIMIT",
            REQUIRED_INITIAL_HISTORY_LIMIT,
        )?;
        if initial_history_limit != REQUIRED_INITIAL_HISTORY_LIMIT {
            return Err(invalid(
                "INITIAL_HISTORY_LIMIT",
                "must be exactly 1000 for the Phase 1 history contract",
            ));
        }
        let backfill_page_limit =
            parse_u16(values, "BACKFILL_PAGE_LIMIT", DEFAULT_BACKFILL_PAGE_LIMIT)?;
        if !(1..=MAX_BACKFILL_PAGE_LIMIT).contains(&backfill_page_limit) {
            return Err(invalid("BACKFILL_PAGE_LIMIT", "must be between 1 and 1500"));
        }
        let analysis_worker_id =
            value_or(values, "ANALYSIS_WORKER_ID", DEFAULT_ANALYSIS_WORKER_ID).to_owned();
        let analysis_lease_seconds = parse_positive_u16(
            values,
            "ANALYSIS_LEASE_SECONDS",
            DEFAULT_ANALYSIS_LEASE_SECONDS,
        )?;
        let analysis_barrier_grace_seconds = parse_positive_u16(
            values,
            "ANALYSIS_BARRIER_GRACE_SECONDS",
            DEFAULT_ANALYSIS_BARRIER_GRACE_SECONDS,
        )?;
        let backtest_max_concurrent_runs = parse_positive_u16(
            values,
            "BACKTEST_MAX_CONCURRENT_RUNS",
            DEFAULT_BACKTEST_MAX_CONCURRENT_RUNS,
        )?;

        Ok(Self {
            bind,
            public_host,
            database_url,
            binance_rest_base_url,
            binance_ws_base_url,
            static_dir: PathBuf::from(value_or(values, "STATIC_DIR", DEFAULT_STATIC_DIR)),
            initial_history_limit,
            backfill_page_limit,
            analysis_worker_id,
            analysis_lease_seconds,
            analysis_barrier_grace_seconds,
            backtest_max_concurrent_runs,
            session_history: SessionHistoryConfig {
                overrides_path: PathBuf::from(value_or(
                    values,
                    "MARKET_SESSION_OVERRIDES_PATH",
                    DEFAULT_SESSION_OVERRIDES_PATH,
                )),
                policy_version: value_or(
                    values,
                    "MARKET_SESSION_POLICY_VERSION",
                    DEFAULT_SESSION_POLICY_VERSION,
                )
                .to_owned(),
            },
        })
    }
}

fn required(values: &HashMap<String, String>, name: &'static str) -> Result<String, ConfigError> {
    values
        .get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(ConfigError::Missing(name))
}

fn value_or<'a>(values: &'a HashMap<String, String>, name: &str, default: &'a str) -> &'a str {
    values
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default)
}

fn parse_socket(
    values: &HashMap<String, String>,
    name: &'static str,
    default: &str,
) -> Result<SocketAddr, ConfigError> {
    value_or(values, name, default)
        .parse()
        .map_err(|error| invalid(name, format!("invalid socket address: {error}")))
}

fn parse_public_host(values: &HashMap<String, String>) -> Result<Option<String>, ConfigError> {
    let host = value_or(values, "APP_PUBLIC_HOST", "")
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        return Ok(None);
    }
    let labels: Vec<&str> = host.split('.').collect();
    let valid_label = |label: &&str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    let numeric_top_level = labels
        .last()
        .is_some_and(|label| label.bytes().all(|byte| byte.is_ascii_digit()));
    if labels.len() < 2 || !labels.iter().all(valid_label) || numeric_top_level {
        return Err(invalid("APP_PUBLIC_HOST", "must be a bare host name"));
    }
    Ok(Some(host))
}

fn parse_url(
    values: &HashMap<String, String>,
    name: &'static str,
    default: &str,
    schemes: &[&str],
) -> Result<Url, ConfigError> {
    let url = Url::parse(value_or(values, name, default))
        .map_err(|error| invalid(name, format!("invalid URL: {error}")))?;
    if !schemes.contains(&url.scheme()) {
        return Err(invalid(
            name,
            format!("scheme must be one of {}", schemes.join(", ")),
        ));
    }
    Ok(url)
}

fn parse_u16(
    values: &HashMap<String, String>,
    name: &'static str,
    default: u16,
) -> Result<u16, ConfigError> {
    match values.get(name) {
        Some(value) if !value.trim().is_empty() => value
            .parse()
            .map_err(|error| invalid(name, format!("invalid integer: {error}"))),
        _ => Ok(default),
    }
}

fn invalid(name: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        message: message.into(),
    }
}

fn parse_positive_u16(
    values: &HashMap<String, String>,
    name: &'static str,
    default: u16,
) -> Result<u16, ConfigError> {
    let value = parse_u16(values, name, default)?;
    if value == 0 {
        return Err(invalid(name, "must be greater than 0"));
    }
    Ok(value)
}
