use gold_monitoring_server::deployment_rollover::{self, DeploymentRolloverArgs};

#[allow(dead_code)]
#[path = "../examples/migrate_from_source.rs"]
mod migrate_from_source;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn deployment_rollover_args_default_to_xauusdt_dry_run() {
    let args = deployment_rollover::parse_args(Vec::<String>::new()).unwrap();
    assert_eq!(
        args,
        DeploymentRolloverArgs {
            symbol: market_domain::Symbol::new("XAUUSDT").unwrap(),
            apply: false,
        }
    );
}

#[test]
fn deployment_rollover_args_accept_symbol_and_apply() {
    let args =
        deployment_rollover::parse_args(strings(&["--symbol", "BTCUSDT", "--apply"])).unwrap();
    assert_eq!(args.symbol.as_str(), "BTCUSDT");
    assert!(args.apply);
}

#[test]
fn deployment_rollover_args_reject_invalid_input() {
    assert!(deployment_rollover::parse_args(strings(&["--symbol", ""])).is_err());
    assert!(deployment_rollover::parse_args(strings(&["--unknown"])).is_err());
}

#[test]
fn migrate_from_source_args_require_url_and_source() {
    assert!(migrate_from_source::parse_args(Vec::<String>::new()).is_err());
    assert!(
        migrate_from_source::parse_args(strings(&["--database-url", "postgres://localhost/test"]))
            .is_err()
    );
    assert!(migrate_from_source::parse_args(strings(&["--source", "migrations"])).is_err());
    assert!(migrate_from_source::parse_args(strings(&["--unknown"])).is_err());
}

#[test]
fn migrate_from_source_args_accept_valid_pair() {
    let args = migrate_from_source::parse_args(strings(&[
        "--database-url",
        "postgres://localhost/test",
        "--source",
        "migrations",
    ]))
    .unwrap();
    assert_eq!(args.database_url, "postgres://localhost/test");
    assert_eq!(args.source.to_string_lossy(), "migrations");
}
