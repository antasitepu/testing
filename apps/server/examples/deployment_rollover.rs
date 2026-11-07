use gold_monitoring_server::{config::AppConfig, deployment_rollover};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = deployment_rollover::parse_args(std::env::args().skip(1))
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let config = AppConfig::from_env()?;
    let report = deployment_rollover::run(&config, &args.symbol, args.apply).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !args.apply && report.initial_open > 0 {
        eprintln!("dry-run only; pass --apply to execute controlled virtual rollover");
    }
    if args.apply && report.remaining_open > 0 {
        return Err(format!(
            "deployment gate remains blocked by {} open trades",
            report.remaining_open
        )
        .into());
    }
    Ok(())
}
