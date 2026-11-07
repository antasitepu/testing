use crate::ServiceError;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use market_domain::{
    AggregateTrade, Candle, ClosedCandleWrite, Market, MarketEvent, MarketSession, Symbol,
    SyncStatus, Timeframe,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSource {
    BinanceApi,
    DerivedWeekly,
    ManualOverride,
}

impl SessionSource {
    pub const fn precedence(self) -> u16 {
        match self {
            Self::DerivedWeekly => 100,
            Self::BinanceApi => 200,
            Self::ManualOverride => 300,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    pub session: MarketSession,
    pub source: SessionSource,
    pub source_reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncCheckpoint {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub oldest_synced_time: i64,
    pub latest_synced_time: i64,
    pub operation: SyncOperation,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncOperation {
    Recent,
    Backfill,
}

#[async_trait]
pub trait CandleStore: Send + Sync {
    async fn upsert_page(&self, candles: &[Candle]) -> Result<(), ServiceError>;
    async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError>;
    async fn persist_closed_operational(
        &self,
        write: &ClosedCandleWrite,
    ) -> Result<(), ServiceError>;
    /// Persists a page of closed candles together with its sync checkpoint.
    /// Stores backed by a database should do this in one transaction.
    async fn persist_closed_page_with_checkpoint(
        &self,
        writes: &[ClosedCandleWrite],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        for write in writes {
            self.persist_closed_operational(write).await?;
        }
        self.persist_page_with_checkpoint(&[], checkpoint).await
    }
    async fn latest(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, ServiceError>;
    async fn bounds(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError>;
    async fn open_times(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<Vec<i64>, ServiceError>;
}

#[async_trait]
pub trait MarketStore: Send + Sync {
    async fn upsert_market(&self, market: &Market) -> Result<(), ServiceError>;
    async fn load_market(&self, symbol: &Symbol) -> Result<Option<Market>, ServiceError>;
    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn upsert_sessions(&self, sessions: &[MarketSession]) -> Result<(), ServiceError>;
    async fn replace_canonical_range(
        &self,
        _session_market: &str,
        _start: i64,
        _end: i64,
        _sessions: &[SessionRecord],
    ) -> Result<(), ServiceError> {
        Err(ServiceError::InvalidData(
            "canonical session replacement is not supported by this store".to_owned(),
        ))
    }
    async fn covering(
        &self,
        session_market: &str,
        timestamp: i64,
    ) -> Result<Option<MarketSession>, ServiceError>;
    async fn between(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<MarketSession>, ServiceError>;
}

#[async_trait]
pub trait SyncStore: Send + Sync {
    async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError>;
    async fn mark_recent_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError>;
    async fn mark_recent_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
        latest: i64,
    ) -> Result<(), ServiceError>;
    async fn mark_backfill_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError>;
    async fn mark_backfill_progress(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
    ) -> Result<(), ServiceError>;
    async fn mark_backfill_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError>;
    async fn mark_failed(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        operation: SyncOperation,
        message: &str,
    ) -> Result<(), ServiceError>;
    async fn all_for_symbol(&self, symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError>;
}

#[async_trait]
pub trait MarketSource: Send + Sync {
    async fn server_time(&self) -> Result<i64, ServiceError>;
    async fn market(&self, symbol: &Symbol) -> Result<Market, ServiceError>;
    async fn klines(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start_time: Option<i64>,
        end_time: Option<i64>,
        limit: u16,
    ) -> Result<Vec<Candle>, ServiceError>;
    async fn aggregate_trades_from(
        &self,
        symbol: &Symbol,
        from_id: i64,
        limit: u16,
    ) -> Result<Vec<AggregateTrade>, ServiceError>;
    async fn trading_schedule(
        &self,
        session_market: &str,
    ) -> Result<Vec<MarketSession>, ServiceError>;
}

#[async_trait]
pub trait MarketStreamSource: Send + Sync {
    async fn connect(
        &self,
        symbol: &Symbol,
    ) -> Result<BoxStream<'static, Result<MarketEvent, ServiceError>>, ServiceError>;
}
