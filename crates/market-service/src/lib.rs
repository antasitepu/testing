#![forbid(unsafe_code)]

mod backfill;
mod error;
mod gap;
mod hub;
mod ports;
mod session_policy;
mod startup;
mod supervisor;
mod sync;

pub use backfill::{BackfillStep, HistoricalBackfill};
pub use error::ServiceError;
pub use gap::{GapReconciler, ReconciliationReport, expected_open_times};
pub use hub::{MarketHub, MarketSnapshot};
pub use ports::{
    CandleStore, MarketSource, MarketStore, MarketStreamSource, SessionRecord, SessionSource,
    SessionStore, SyncCheckpoint, SyncOperation, SyncStore,
};
pub use session_policy::{
    CandleSessionClass, CommoditySessionGenerator, TradableCandlePolicy, canonicalize_sessions,
};
pub use startup::{StartupCoordinator, StartupOutcome};
pub use supervisor::{StreamSupervisor, reconnect_delay};
pub use sync::RecentSync;
