#![forbid(unsafe_code)]

mod ambiguity;
mod event;
mod health;
mod projection;
mod trade;

pub use ambiguity::{
    AmbiguityDecision, AmbiguousOutcome, ExecutionKind, OhlcExecutionRange,
    classify_ohlc_ambiguity, known_ordered_outcome,
};
pub use event::{VirtualTradeEvent, VirtualTradeEventKind};
pub use health::TrackingHealth;
pub use projection::MonitoringEvent;
pub use trade::{
    Ambiguity, ExecutionModel, MarketCursor, PositionStage, ReconciliationSource, TrackedLevelKind,
    TradeHealth, TradeState, VirtualTrade,
};
