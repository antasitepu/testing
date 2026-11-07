use crate::{
    CandleRepository, MarketRepository, SessionRepository, StorageCheckpoint, StorageError,
    StorageSyncOperation, SyncRepository,
};
use async_trait::async_trait;
use market_domain::{
    Candle, ClosedCandleWrite, Market, MarketSession, Symbol, SyncStatus, Timeframe,
};
use market_service::{
    CandleStore, MarketStore, ServiceError, SessionStore, SyncCheckpoint, SyncOperation, SyncStore,
};

fn storage_error(operation: &'static str, error: StorageError) -> ServiceError {
    ServiceError::Storage {
        operation,
        message: error.to_string(),
    }
}

fn storage_checkpoint(checkpoint: &SyncCheckpoint) -> StorageCheckpoint {
    StorageCheckpoint {
        symbol: checkpoint.symbol.clone(),
        timeframe: checkpoint.timeframe,
        oldest_synced_time: checkpoint.oldest_synced_time,
        latest_synced_time: checkpoint.latest_synced_time,
        operation: match checkpoint.operation {
            SyncOperation::Recent => StorageSyncOperation::Recent,
            SyncOperation::Backfill => StorageSyncOperation::Backfill,
        },
        complete: checkpoint.complete,
    }
}

#[async_trait]
impl CandleStore for CandleRepository {
    async fn upsert_page(&self, candles: &[Candle]) -> Result<(), ServiceError> {
        CandleRepository::upsert_page(self, candles)
            .await
            .map_err(|error| storage_error("upsert candle page", error))
    }

    async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        CandleRepository::persist_page_with_checkpoint(
            self,
            candles,
            &storage_checkpoint(checkpoint),
        )
        .await
        .map_err(|error| storage_error("persist candle page with checkpoint", error))
    }

    async fn persist_closed_operational(
        &self,
        write: &ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        CandleRepository::persist_closed_operational(self, write)
            .await
            .map_err(|error| storage_error("persist closed candle", error))
    }

    async fn persist_closed_page_with_checkpoint(
        &self,
        writes: &[ClosedCandleWrite],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        CandleRepository::persist_closed_page_with_checkpoint(
            self,
            writes,
            &storage_checkpoint(checkpoint),
        )
        .await
        .map_err(|error| storage_error("persist closed candle page", error))
    }

    async fn latest(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        CandleRepository::latest(self, symbol, timeframe, before, limit)
            .await
            .map_err(|error| storage_error("load latest candles", error))
    }

    async fn bounds(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        CandleRepository::bounds(self, symbol, timeframe)
            .await
            .map_err(|error| storage_error("load candle bounds", error))
    }

    async fn open_times(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        CandleRepository::open_times(self, symbol, timeframe, start, end)
            .await
            .map_err(|error| storage_error("load candle open times", error))
    }
}

#[async_trait]
impl MarketStore for MarketRepository {
    async fn upsert_market(&self, market: &Market) -> Result<(), ServiceError> {
        MarketRepository::upsert(self, market)
            .await
            .map_err(|error| storage_error("upsert market", error))
    }

    async fn load_market(&self, symbol: &Symbol) -> Result<Option<Market>, ServiceError> {
        MarketRepository::load(self, symbol)
            .await
            .map_err(|error| storage_error("load market", error))
    }

    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError> {
        MarketRepository::enabled(self)
            .await
            .map_err(|error| storage_error("load enabled markets", error))
    }
}

#[async_trait]
impl SessionStore for SessionRepository {
    async fn upsert_sessions(&self, sessions: &[MarketSession]) -> Result<(), ServiceError> {
        SessionRepository::upsert_sessions(self, sessions)
            .await
            .map_err(|error| storage_error("upsert market sessions", error))
    }

    async fn replace_canonical_range(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
        sessions: &[market_service::SessionRecord],
    ) -> Result<(), ServiceError> {
        SessionRepository::replace_canonical_range(self, session_market, start, end, sessions)
            .await
            .map_err(|error| storage_error("replace canonical market sessions", error))
    }

    async fn covering(
        &self,
        session_market: &str,
        timestamp: i64,
    ) -> Result<Option<MarketSession>, ServiceError> {
        SessionRepository::covering(self, session_market, timestamp)
            .await
            .map_err(|error| storage_error("load covering market session", error))
    }

    async fn between(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<MarketSession>, ServiceError> {
        SessionRepository::between(self, session_market, start, end)
            .await
            .map_err(|error| storage_error("load market sessions", error))
    }
}

#[async_trait]
impl SyncStore for SyncRepository {
    async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        SyncRepository::load(self, symbol, timeframe)
            .await
            .map_err(|error| storage_error("load sync state", error))
    }

    async fn mark_recent_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_recent_running(self, symbol, timeframe)
            .await
            .map_err(|error| storage_error("mark recent sync running", error))
    }

    async fn mark_recent_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
        latest: i64,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_recent_complete(self, symbol, timeframe, oldest, latest)
            .await
            .map_err(|error| storage_error("mark recent sync complete", error))
    }

    async fn mark_backfill_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_backfill_running(self, symbol, timeframe)
            .await
            .map_err(|error| storage_error("mark backfill running", error))
    }

    async fn mark_backfill_progress(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_backfill_progress(self, symbol, timeframe, oldest)
            .await
            .map_err(|error| storage_error("mark backfill progress", error))
    }

    async fn mark_backfill_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_backfill_complete(self, symbol, timeframe)
            .await
            .map_err(|error| storage_error("mark backfill complete", error))
    }

    async fn mark_failed(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        operation: SyncOperation,
        message: &str,
    ) -> Result<(), ServiceError> {
        SyncRepository::mark_failed(self, symbol, timeframe, operation, message)
            .await
            .map_err(|error| storage_error("mark sync failed", error))
    }

    async fn all_for_symbol(&self, symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError> {
        SyncRepository::all_for_symbol(self, symbol)
            .await
            .map_err(|error| storage_error("load symbol sync states", error))
    }
}
