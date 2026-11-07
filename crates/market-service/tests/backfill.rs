use async_trait::async_trait;
use market_domain::{
    AggregateTrade, Candle, LiveStatus, Market, MarketSession, MarketStatus, Symbol, SyncRunStatus,
    SyncStatus, Timeframe,
};
use market_service::{
    BackfillStep, CandleStore, HistoricalBackfill, MarketHub, MarketSource, ServiceError,
    SyncCheckpoint, SyncOperation, SyncStore,
};
use rust_decimal::Decimal;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

struct FakeSource {
    pages: Mutex<VecDeque<Vec<Candle>>>,
    end_times: Mutex<Vec<Option<i64>>>,
}

#[async_trait]
impl MarketSource for FakeSource {
    async fn server_time(&self) -> Result<i64, ServiceError> {
        Ok(0)
    }

    async fn market(&self, _symbol: &Symbol) -> Result<Market, ServiceError> {
        unreachable!()
    }

    async fn klines(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _start_time: Option<i64>,
        end_time: Option<i64>,
        _limit: u16,
    ) -> Result<Vec<Candle>, ServiceError> {
        self.end_times.lock().unwrap().push(end_time);
        Ok(self.pages.lock().unwrap().pop_front().unwrap_or_default())
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
        Ok(Vec::new())
    }
}

struct FakeCandles {
    bounds: Mutex<Option<(i64, i64)>>,
    operations: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl CandleStore for FakeCandles {
    async fn upsert_page(&self, _candles: &[Candle]) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        self.operations
            .lock()
            .unwrap()
            .push(format!("commit:{}", checkpoint.oldest_synced_time));
        let page_oldest = candles.iter().map(|item| item.open_time).min().unwrap();
        let mut bounds = self.bounds.lock().unwrap();
        let latest = bounds.map_or(page_oldest, |current| current.1);
        *bounds = Some((page_oldest, latest));
        Ok(())
    }

    async fn persist_closed_operational(
        &self,
        _write: &market_domain::ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn latest(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _before: Option<i64>,
        _limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        Ok(Vec::new())
    }

    async fn bounds(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        Ok(*self.bounds.lock().unwrap())
    }

    async fn open_times(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _start: i64,
        _end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        Ok(Vec::new())
    }
}

struct FakeSync {
    states: Mutex<HashMap<(Symbol, Timeframe), SyncStatus>>,
    operations: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SyncStore for FakeSync {
    async fn load(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<Option<SyncStatus>, ServiceError> {
        Ok(self
            .states
            .lock()
            .unwrap()
            .get(&(symbol.clone(), timeframe))
            .cloned())
    }

    async fn mark_recent_running(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn mark_recent_complete(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        _oldest: i64,
        _latest: i64,
    ) -> Result<(), ServiceError> {
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
        symbol: &Symbol,
        timeframe: Timeframe,
        oldest: i64,
    ) -> Result<(), ServiceError> {
        self.operations
            .lock()
            .unwrap()
            .push(format!("progress:{oldest}"));
        self.states
            .lock()
            .unwrap()
            .get_mut(&(symbol.clone(), timeframe))
            .unwrap()
            .oldest_synced_time = Some(oldest);
        Ok(())
    }

    async fn mark_backfill_complete(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
    ) -> Result<(), ServiceError> {
        let mut states = self.states.lock().unwrap();
        let state = states.get_mut(&(symbol.clone(), timeframe)).unwrap();
        state.full_history_complete = true;
        state.backfill_status = SyncRunStatus::Complete;
        Ok(())
    }

    async fn mark_failed(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        _operation: SyncOperation,
        message: &str,
    ) -> Result<(), ServiceError> {
        let mut states = self.states.lock().unwrap();
        let state = states.get_mut(&(symbol.clone(), timeframe)).unwrap();
        state.backfill_status = SyncRunStatus::Failed;
        state.last_error = Some(message.to_owned());
        Ok(())
    }

    async fn all_for_symbol(&self, symbol: &Symbol) -> Result<Vec<SyncStatus>, ServiceError> {
        Ok(self
            .states
            .lock()
            .unwrap()
            .values()
            .filter(|status| &status.symbol == symbol)
            .cloned()
            .collect())
    }
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn market() -> Market {
    Market::new(
        symbol(),
        true,
        MarketStatus::Trading,
        Decimal::new(1, 2),
        0,
        "COMMODITY",
    )
    .unwrap()
}

fn candle(open_time: i64) -> Candle {
    Candle::new(
        symbol(),
        Timeframe::M1,
        open_time,
        open_time + 59,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        Decimal::new(410050, 2),
    )
    .unwrap()
}

fn harness(
    pages: Vec<Vec<Candle>>,
) -> (
    Arc<FakeSource>,
    Arc<FakeCandles>,
    Arc<FakeSync>,
    HistoricalBackfill,
) {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(FakeSource {
        pages: Mutex::new(pages.into()),
        end_times: Mutex::new(Vec::new()),
    });
    let candles = Arc::new(FakeCandles {
        bounds: Mutex::new(Some((300, 600))),
        operations: operations.clone(),
    });
    let sync = Arc::new(FakeSync {
        states: Mutex::new(HashMap::from([(
            (symbol(), Timeframe::M1),
            SyncStatus {
                symbol: symbol(),
                timeframe: Timeframe::M1,
                oldest_synced_time: Some(300),
                latest_synced_time: Some(600),
                full_history_complete: false,
                recent_sync_status: SyncRunStatus::Complete,
                backfill_status: SyncRunStatus::Pending,
                last_error: None,
            },
        )])),
        operations,
    });
    let backfill = HistoricalBackfill::new(source.clone(), candles.clone(), sync.clone(), 1_500);
    (source, candles, sync, backfill)
}

#[tokio::test]
async fn pages_move_backward_and_restart_uses_persisted_checkpoint() {
    let (source, _candles, sync, backfill) =
        harness(vec![vec![candle(100), candle(200)], vec![candle(0)]]);

    assert_eq!(
        backfill.step(&market(), Timeframe::M1).await.unwrap(),
        BackfillStep::Progress {
            oldest_open_time: 100,
            rows: 2
        }
    );
    assert_eq!(
        sync.operations.lock().unwrap().as_slice(),
        ["commit:100", "progress:100"]
    );

    let restarted = HistoricalBackfill::new(source.clone(), _candles.clone(), sync.clone(), 1_500);
    assert_eq!(
        restarted.step(&market(), Timeframe::M1).await.unwrap(),
        BackfillStep::Complete
    );
    assert_eq!(
        source.end_times.lock().unwrap().as_slice(),
        [Some(299), Some(99)]
    );
    assert!(
        sync.states
            .lock()
            .unwrap()
            .get(&(symbol(), Timeframe::M1))
            .unwrap()
            .full_history_complete
    );
}

#[tokio::test]
async fn empty_page_marks_full_history_complete_without_health_change() {
    let (_source, _candles, sync, backfill) = harness(vec![Vec::new()]);
    let hub = MarketHub::new(8);
    hub.set_status(&symbol(), LiveStatus::Ready).await;

    let step = backfill.step(&market(), Timeframe::M1).await.unwrap();

    assert_eq!(step, BackfillStep::Complete);
    let state = sync
        .states
        .lock()
        .unwrap()
        .get(&(symbol(), Timeframe::M1))
        .unwrap()
        .clone();
    assert_eq!(state.backfill_status, SyncRunStatus::Complete);
    assert!(state.last_error.is_none());
    assert_eq!(
        hub.snapshot(&symbol(), Timeframe::M1).await.status,
        LiveStatus::Ready
    );
}
