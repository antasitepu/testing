use async_trait::async_trait;
use market_domain::{
    AggregateTrade, Candle, ClosedCandleSource, LiveStatus, Market, MarketSession, MarketStatus,
    SessionStatus, Symbol, SyncRunStatus, SyncStatus, Timeframe,
};
use market_service::{
    CandleStore, MarketHub, MarketSource, MarketStore, RecentSync, ServiceError, SessionStore,
    StartupCoordinator, SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const SERVER_TIME: i64 = 1_800_000_000;

#[derive(Default)]
struct FakeCandleStore {
    rows: Mutex<HashMap<(Symbol, Timeframe), Vec<Candle>>>,
    checkpoints: Mutex<Vec<SyncCheckpoint>>,
    operational_sources: Mutex<Vec<ClosedCandleSource>>,
}

impl FakeCandleStore {
    fn seed(&self, candle: Candle) {
        self.rows
            .lock()
            .unwrap()
            .entry((candle.symbol.clone(), candle.timeframe))
            .or_default()
            .push(candle);
    }

    fn count(&self) -> usize {
        self.rows.lock().unwrap().values().map(Vec::len).sum()
    }
}

#[async_trait]
impl CandleStore for FakeCandleStore {
    async fn upsert_page(&self, candles: &[Candle]) -> Result<(), ServiceError> {
        self.persist(candles);
        Ok(())
    }

    async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        self.persist(candles);
        self.checkpoints.lock().unwrap().push(checkpoint.clone());
        Ok(())
    }

    async fn persist_closed_operational(
        &self,
        write: &market_domain::ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        self.operational_sources.lock().unwrap().push(write.source);
        self.persist(std::slice::from_ref(&write.candle));
        Ok(())
    }

    async fn latest(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        let mut rows = self
            .rows
            .lock()
            .unwrap()
            .get(&(symbol.clone(), timeframe))
            .cloned()
            .unwrap_or_default();
        rows.retain(|candle| before.is_none_or(|cursor| candle.open_time < cursor));
        rows.sort_by_key(|candle| candle.open_time);
        let start = rows.len().saturating_sub(limit as usize);
        Ok(rows[start..].to_vec())
    }

    async fn bounds(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        let rows = self.rows.lock().unwrap();
        let rows = rows.get(&(symbol.clone(), timeframe));
        Ok(rows.and_then(|items| {
            let oldest = items.iter().map(|item| item.open_time).min()?;
            let latest = items.iter().map(|item| item.open_time).max()?;
            Some((oldest, latest))
        }))
    }

    async fn open_times(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(&(symbol.clone(), timeframe))
            .into_iter()
            .flatten()
            .filter(|item| item.open_time >= start && item.open_time < end)
            .map(|item| item.open_time)
            .collect())
    }
}

impl FakeCandleStore {
    fn persist(&self, candles: &[Candle]) {
        let mut rows = self.rows.lock().unwrap();
        for candle in candles {
            let values = rows
                .entry((candle.symbol.clone(), candle.timeframe))
                .or_default();
            if let Some(current) = values
                .iter_mut()
                .find(|current| current.open_time == candle.open_time)
            {
                *current = candle.clone();
            } else {
                values.push(candle.clone());
            }
        }
    }
}

#[derive(Default)]
struct FakeSyncStore {
    statuses: Mutex<HashMap<(Symbol, Timeframe), SyncStatus>>,
}

#[async_trait]
impl SyncStore for FakeSyncStore {
    async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        Ok(self
            .statuses
            .lock()
            .unwrap()
            .get(&(symbol.clone(), timeframe))
            .cloned())
    }

    async fn mark_recent_running(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        self.set_status(symbol, timeframe, SyncRunStatus::Running, None, None);
        Ok(())
    }

    async fn mark_recent_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
        latest: i64,
    ) -> Result<(), ServiceError> {
        self.set_status(
            symbol,
            timeframe,
            SyncRunStatus::Complete,
            Some(oldest),
            Some(latest),
        );
        Ok(())
    }

    async fn mark_backfill_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_backfill_progress(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_backfill_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_failed(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _operation: SyncOperation,
        _message: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn all_for_symbol(&self, symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError> {
        Ok(self
            .statuses
            .lock()
            .unwrap()
            .values()
            .filter(|status| &status.symbol == symbol)
            .cloned()
            .collect())
    }
}

impl FakeSyncStore {
    fn set_status(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        recent_sync_status: SyncRunStatus,
        oldest: Option<i64>,
        latest: Option<i64>,
    ) {
        self.statuses.lock().unwrap().insert(
            (symbol.clone(), timeframe),
            SyncStatus {
                symbol: symbol.clone(),
                timeframe,
                oldest_synced_time: oldest,
                latest_synced_time: latest,
                full_history_complete: false,
                recent_sync_status,
                backfill_status: SyncRunStatus::Pending,
                last_error: None,
            },
        );
    }
}

struct FakeSource {
    sessions: Vec<MarketSession>,
    calls: Mutex<Vec<(Timeframe, Option<i64>, u16)>>,
    fail_timeframe: Option<Timeframe>,
}

#[async_trait]
impl MarketSource for FakeSource {
    async fn server_time(&self) -> Result<i64, ServiceError> {
        Ok(SERVER_TIME)
    }

    async fn market(&self, symbol: &Symbol) -> Result<Market, ServiceError> {
        Market::new(
            symbol.clone(),
            true,
            MarketStatus::Trading,
            Decimal::new(1, 2),
            0,
            "COMMODITY",
        )
        .map_err(|error| ServiceError::InvalidData(error.to_string()))
    }

    async fn klines(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start_time: Option<i64>,
        _end_time: Option<i64>,
        limit: u16,
    ) -> Result<Vec<Candle>, ServiceError> {
        self.calls
            .lock()
            .unwrap()
            .push((timeframe, start_time, limit));
        if self.fail_timeframe == Some(timeframe) {
            return Err(ServiceError::Source {
                operation: "fake klines",
                message: "injected failure".to_owned(),
            });
        }
        let open_time = start_time.unwrap_or(SERVER_TIME - timeframe.duration_ms());
        Ok(vec![closed_candle(symbol, timeframe, open_time)])
    }

    async fn aggregate_trades_from(
        &self,
        _symbol: &Symbol,
        _from_id: i64,
        _limit: u16,
    ) -> Result<Vec<AggregateTrade>, ServiceError> {
        Ok(Vec::new())
    }

    async fn trading_schedule(
        &self,
        _session_market: &str,
    ) -> Result<Vec<MarketSession>, ServiceError> {
        Ok(self.sessions.clone())
    }
}

#[derive(Default)]
struct FakeMarketStore {
    values: Mutex<Vec<Market>>,
}

#[async_trait]
impl MarketStore for FakeMarketStore {
    async fn upsert_market(&self, market: &Market) -> Result<(), ServiceError> {
        self.values.lock().unwrap().push(market.clone());
        Ok(())
    }

    async fn load_market(&self, symbol: &Symbol) -> Result<Option<Market>, ServiceError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .iter()
            .find(|market| &market.symbol == symbol)
            .cloned())
    }

    async fn enabled_markets(&self) -> Result<Vec<Market>, ServiceError> {
        Ok(self.values.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct FakeSessionStore {
    values: Mutex<Vec<MarketSession>>,
}

#[async_trait]
impl SessionStore for FakeSessionStore {
    async fn upsert_sessions(&self, sessions: &[MarketSession]) -> Result<(), ServiceError> {
        *self.values.lock().unwrap() = sessions.to_vec();
        Ok(())
    }

    async fn covering(
        &self,
        session_market: &str,
        timestamp: i64,
    ) -> Result<Option<MarketSession>, ServiceError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .iter()
            .find(|session| session.session_market == session_market && session.contains(timestamp))
            .cloned())
    }

    async fn between(
        &self,
        session_market: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<MarketSession>, ServiceError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .iter()
            .filter(|session| {
                session.session_market == session_market
                    && session.end_time > start
                    && session.start_time < end
            })
            .cloned()
            .collect())
    }
}

fn closed_candle(symbol: &Symbol, timeframe: Timeframe, open_time: i64) -> Candle {
    Candle::new(
        symbol.clone(),
        timeframe,
        open_time,
        open_time + timeframe.duration_ms() - 1,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        Decimal::new(410050, 2),
    )
    .unwrap()
}

fn session(status: SessionStatus) -> MarketSession {
    MarketSession::new(
        "COMMODITY",
        status,
        SERVER_TIME - 10_000,
        SERVER_TIME + 10_000,
        SERVER_TIME,
    )
    .unwrap()
}

fn source(status: SessionStatus, fail_timeframe: Option<Timeframe>) -> Arc<FakeSource> {
    Arc::new(FakeSource {
        sessions: vec![session(status)],
        calls: Mutex::new(Vec::new()),
        fail_timeframe,
    })
}

#[tokio::test]
async fn first_startup_syncs_all_timeframes_before_ready() {
    let source = source(SessionStatus::Regular, None);
    let candles = Arc::new(FakeCandleStore::default());
    let sync = Arc::new(FakeSyncStore::default());
    let hub = Arc::new(MarketHub::new(32));
    let coordinator = StartupCoordinator::new(
        source.clone(),
        Arc::new(FakeMarketStore::default()),
        Arc::new(FakeSessionStore::default()),
        Arc::new(RecentSync::new(
            source.clone(),
            candles.clone(),
            sync,
            1_000,
            1_500,
        )),
        hub.clone(),
    );

    let outcome = coordinator
        .initialize(Symbol::new("XAUUSDT").unwrap())
        .await
        .unwrap();

    assert_eq!(outcome.final_status, LiveStatus::Ready);
    assert_eq!(outcome.backfill_timeframes, Timeframe::ALL);
    assert_eq!(candles.count(), 7);
    assert_eq!(
        candles.operational_sources.lock().unwrap().as_slice(),
        [ClosedCandleSource::CatchUp; 7]
    );
    assert_eq!(source.calls.lock().unwrap().len(), 7);
    let synced_order: Vec<_> = source
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(timeframe, _, _)| *timeframe)
        .collect();
    let mut higher_first = Timeframe::ALL.to_vec();
    higher_first.reverse();
    assert_eq!(synced_order, higher_first);
    assert!(
        source
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, start, limit)| start.is_none() && *limit == 1_000)
    );
    assert_eq!(
        hub.snapshot(&Symbol::new("XAUUSDT").unwrap(), Timeframe::M1)
            .await
            .status,
        LiveStatus::Ready
    );
}

#[tokio::test]
async fn subsequent_sync_starts_after_latest_open_without_duplicates() {
    let source = source(SessionStatus::Regular, None);
    let candles = Arc::new(FakeCandleStore::default());
    let symbol = Symbol::new("XAUUSDT").unwrap();
    candles.seed(closed_candle(&symbol, Timeframe::M1, 60_000));
    let sync = RecentSync::new(
        source.clone(),
        candles.clone(),
        Arc::new(FakeSyncStore::default()),
        1_000,
        1_500,
    );

    sync.run(&symbol, Timeframe::M1, SERVER_TIME).await.unwrap();
    sync.run(&symbol, Timeframe::M1, SERVER_TIME).await.unwrap();

    let calls = source.calls.lock().unwrap();
    assert_eq!(calls[0], (Timeframe::M1, Some(120_000), 1_500));
    assert_eq!(calls[1], (Timeframe::M1, Some(180_000), 1_500));
    assert_eq!(candles.count(), 3);
}

#[tokio::test]
async fn one_timeframe_failure_keeps_startup_syncing() {
    let source = source(SessionStatus::Regular, Some(Timeframe::M15));
    let hub = Arc::new(MarketHub::new(32));
    let coordinator = StartupCoordinator::new(
        source.clone(),
        Arc::new(FakeMarketStore::default()),
        Arc::new(FakeSessionStore::default()),
        Arc::new(RecentSync::new(
            source,
            Arc::new(FakeCandleStore::default()),
            Arc::new(FakeSyncStore::default()),
            1_000,
            1_500,
        )),
        hub.clone(),
    );
    let symbol = Symbol::new("XAUUSDT").unwrap();

    assert!(coordinator.initialize(symbol.clone()).await.is_err());
    assert_eq!(
        hub.snapshot(&symbol, Timeframe::M1).await.status,
        LiveStatus::Syncing
    );
}

#[tokio::test]
async fn no_trading_session_finishes_as_market_closed() {
    let source = source(SessionStatus::NoTrading, None);
    let hub = Arc::new(MarketHub::new(32));
    let coordinator = StartupCoordinator::new(
        source.clone(),
        Arc::new(FakeMarketStore::default()),
        Arc::new(FakeSessionStore::default()),
        Arc::new(RecentSync::new(
            source,
            Arc::new(FakeCandleStore::default()),
            Arc::new(FakeSyncStore::default()),
            1_000,
            1_500,
        )),
        hub,
    );

    let outcome = coordinator
        .initialize(Symbol::new("XAUUSDT").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.final_status, LiveStatus::MarketClosed);
}
