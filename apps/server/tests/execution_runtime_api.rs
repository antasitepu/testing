use execution_service::TrackerStore;
use gold_monitoring_server::{
    execution_wake::{ExecutionBarrierRequest, ExecutionWakeSource},
    runtime::{execution::ExecutionRuntime, health::RuntimeHealth},
};
use market_domain::{MarketEvent, Symbol};
use market_service::MarketSource;
use market_storage::CandleRepository;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch};

#[test]
fn execution_runtime_is_constructible_from_the_public_api() {
    #[allow(clippy::too_many_arguments)]
    fn construct(
        symbol: Symbol,
        pool: PgPool,
        rest: Arc<dyn MarketSource>,
        candles: Arc<CandleRepository>,
        tracker: Arc<dyn TrackerStore>,
        market_events: broadcast::Receiver<MarketEvent>,
        input_wake: watch::Receiver<u64>,
        barrier_requests: mpsc::Receiver<ExecutionBarrierRequest>,
        monitoring_wake: ExecutionWakeSource,
        shutdown: watch::Receiver<bool>,
        health: RuntimeHealth,
    ) -> ExecutionRuntime {
        ExecutionRuntime {
            symbol,
            pool,
            rest,
            candles,
            tracker,
            market_events,
            input_wake,
            barrier_requests,
            monitoring_wake,
            shutdown,
            health,
        }
    }

    let _ = construct;
}
