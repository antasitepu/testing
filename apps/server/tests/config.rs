use gold_monitoring_server::config::{AppConfig, ConfigError};
use std::collections::HashMap;

fn values() -> HashMap<String, String> {
    HashMap::from([(
        "DATABASE_URL".to_owned(),
        "postgres://localhost/gold_monitoring".to_owned(),
    )])
}

#[test]
fn production_defaults_are_loopback_and_use_binance_futures() {
    let config = AppConfig::from_values(&values()).unwrap();

    assert!(config.bind.ip().is_loopback());
    assert_eq!(config.bind.port(), 3000);
    assert_eq!(
        config.binance_rest_base_url.as_str(),
        "https://fapi.binance.com/"
    );
    assert_eq!(
        config.binance_ws_base_url.as_str(),
        "wss://fstream.binance.com/market"
    );
    assert_eq!(config.initial_history_limit, 1_000);
    assert_eq!(config.backfill_page_limit, 1_500);
    assert_eq!(config.analysis_worker_id, "local-1");
    assert_eq!(config.analysis_lease_seconds, 30);
    assert_eq!(config.analysis_barrier_grace_seconds, 10);
    assert_eq!(config.backtest_max_concurrent_runs, 1);
    assert_eq!(
        config.session_history.overrides_path,
        std::path::PathBuf::from("config/commodity-session-overrides.json")
    );
    assert_eq!(
        config.session_history.policy_version,
        "commodity-session-policy-v2"
    );
}

#[test]
fn database_url_is_required_and_must_not_be_blank() {
    assert!(matches!(
        AppConfig::from_values(&HashMap::new()),
        Err(ConfigError::Missing("DATABASE_URL"))
    ));
    let mut blank = values();
    blank.insert("DATABASE_URL".to_owned(), "  ".to_owned());
    assert!(matches!(
        AppConfig::from_values(&blank),
        Err(ConfigError::Missing("DATABASE_URL"))
    ));
}

#[test]
fn bind_address_must_be_loopback() {
    let mut config = values();
    config.insert("APP_BIND".to_owned(), "0.0.0.0:3000".to_owned());

    assert!(matches!(
        AppConfig::from_values(&config),
        Err(ConfigError::NonLoopback)
    ));
}

#[test]
fn public_host_is_optional_and_must_be_a_bare_host_name() {
    assert_eq!(AppConfig::from_values(&values()).unwrap().public_host, None);

    let mut public = values();
    public.insert("APP_PUBLIC_HOST".to_owned(), "Trade.Example.com".to_owned());
    assert_eq!(
        AppConfig::from_values(&public)
            .unwrap()
            .public_host
            .as_deref(),
        Some("trade.example.com")
    );

    for bad in [
        "https://trade.example.com",
        "trade.example.com:443",
        "trade.example.com/path",
        "localhost",
        "10.0.0.5",
        "a..example.com",
        "-a.example.com",
    ] {
        let mut config = values();
        config.insert("APP_PUBLIC_HOST".to_owned(), bad.to_owned());
        assert!(
            matches!(
                AppConfig::from_values(&config),
                Err(ConfigError::Invalid {
                    name: "APP_PUBLIC_HOST",
                    ..
                })
            ),
            "{bad}"
        );
    }
}

#[test]
fn history_limits_are_bounded() {
    let mut initial = values();
    initial.insert("INITIAL_HISTORY_LIMIT".to_owned(), "999".to_owned());
    assert!(matches!(
        AppConfig::from_values(&initial),
        Err(ConfigError::Invalid {
            name: "INITIAL_HISTORY_LIMIT",
            ..
        })
    ));

    let mut backfill = values();
    backfill.insert("BACKFILL_PAGE_LIMIT".to_owned(), "1501".to_owned());
    assert!(matches!(
        AppConfig::from_values(&backfill),
        Err(ConfigError::Invalid {
            name: "BACKFILL_PAGE_LIMIT",
            ..
        })
    ));

    let mut workers = values();
    workers.insert("BACKTEST_MAX_CONCURRENT_RUNS".to_owned(), "0".to_owned());
    assert!(matches!(
        AppConfig::from_values(&workers),
        Err(ConfigError::Invalid {
            name: "BACKTEST_MAX_CONCURRENT_RUNS",
            ..
        })
    ));
}

#[test]
fn endpoint_schemes_are_validated() {
    let mut rest = values();
    rest.insert(
        "BINANCE_REST_BASE_URL".to_owned(),
        "file:///tmp/binance".to_owned(),
    );
    assert!(matches!(
        AppConfig::from_values(&rest),
        Err(ConfigError::Invalid {
            name: "BINANCE_REST_BASE_URL",
            ..
        })
    ));

    let mut websocket = values();
    websocket.insert(
        "BINANCE_WS_BASE_URL".to_owned(),
        "https://fstream.binance.com/market".to_owned(),
    );
    assert!(matches!(
        AppConfig::from_values(&websocket),
        Err(ConfigError::Invalid {
            name: "BINANCE_WS_BASE_URL",
            ..
        })
    ));
}
