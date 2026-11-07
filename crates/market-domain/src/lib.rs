#![forbid(unsafe_code)]

mod candle;
mod event;
mod market;
mod status;
mod timeframe;

pub use candle::{Candle, CandleError};
pub use event::{
    AggregateTrade, AggregateTradeError, ClosedCandleSource, ClosedCandleWrite, MarketEvent,
};
pub use market::{
    Market, MarketError, MarketSession, MarketSessionError, MarketStatus, SessionStatus, Symbol,
    SymbolError,
};
pub use status::{LiveStatus, SyncRunStatus, SyncStatus};
pub use timeframe::{Timeframe, TimeframeParseError};
