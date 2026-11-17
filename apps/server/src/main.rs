#![forbid(unsafe_code)]

use gold_monitoring_server::{
    auth::{AuthService, DEFAULT_DEVICE_LIMIT, Role},
    config::AppConfig,
    runtime,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();
    let config = AppConfig::from_env()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [] => runtime::run(config, shutdown_signal()).await,
        [command, username] if command == "create-admin" => create_admin(&config, username).await,
        _ => Err("usage: gold-monitoring-server [create-admin <username>]".into()),
    }
}

/// Creates an admin whose password is the first line of standard input.
async fn create_admin(
    config: &AppConfig,
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&config.database_url)
        .await?;
    AuthService::new(pool, false)
        .create_user(
            username,
            password.trim_end_matches(['\r', '\n']),
            Role::Admin,
            DEFAULT_DEVICE_LIMIT,
            "cli",
        )
        .await?;
    println!("admin {username} created");
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for Ctrl-C");
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
