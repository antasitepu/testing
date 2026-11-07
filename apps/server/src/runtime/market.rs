use super::{
    current_unix_millis,
    health::RuntimeHealth,
    tasks::{publish_panic, stop_service_task},
};
use market_domain::Symbol;
use market_service::{HistoricalBackfill, ServiceError, StartupCoordinator, StreamSupervisor};
use tokio::{sync::watch, task::JoinHandle};

pub fn spawn_market_runtime(
    startup: StartupCoordinator,
    supervisor: StreamSupervisor,
    backfill: HistoricalBackfill,
    market_symbol: Symbol,
    mut market_shutdown: watch::Receiver<bool>,
    market_health: RuntimeHealth,
) -> JoinHandle<Result<(), ServiceError>> {
    tokio::spawn(async move {
        let result = async {
            let startup_outcome = startup.initialize(market_symbol.clone()).await?;
            let supervisor_shutdown = market_shutdown.clone();
            let supervisor_health = market_health.clone();
            let mut supervisor_task = tokio::spawn(async move {
                let result = publish_panic(
                    &supervisor_health,
                    "websocket supervisor",
                    supervisor.run(market_symbol, supervisor_shutdown),
                )
                .await;
                if let Err(error) = &result {
                    supervisor_health.publish_task_failure(
                        format!("websocket supervisor failed: {error}"),
                        current_unix_millis(),
                    );
                }
                result
            });
            let backfill_shutdown = market_shutdown.clone();
            let backfill_health = market_health.clone();
            let mut backfill_task = tokio::spawn(async move {
                let result = publish_panic(
                    &backfill_health,
                    "historical backfill",
                    backfill.run_round_robin(
                        startup_outcome.market,
                        startup_outcome.backfill_timeframes,
                        backfill_shutdown,
                    ),
                )
                .await;
                if let Err(error) = &result {
                    backfill_health.publish_task_failure(
                        format!("historical backfill failed: {error}"),
                        current_unix_millis(),
                    );
                }
                result
            });
            while !*market_shutdown.borrow() {
                if market_shutdown.changed().await.is_err() {
                    break;
                }
            }
            stop_service_task("websocket supervisor", &mut supervisor_task).await;
            stop_service_task("historical backfill", &mut backfill_task).await;
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            market_health.publish_task_failure(
                format!("market runtime failed: {error}"),
                current_unix_millis(),
            );
        }
        result
    })
}
