#![forbid(unsafe_code)]

use gold_monitoring_server::{config::AppConfig, runtime};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();
    let config = AppConfig::from_env()?;
    runtime::run(config, shutdown_signal()).await
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
