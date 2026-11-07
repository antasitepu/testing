use async_trait::async_trait;
use market_domain::{
    AggregateTrade, Candle, ClosedCandleSource, LiveStatus, Market, MarketSession, MarketStatus,
    SessionStatus, Symbol, Timeframe,
};
use market_service::{
    CandleStore, GapReconciler, MarketHub, MarketSource, ServiceError, SessionStore,
    SyncCheckpoint, expected_open_times,
};
use rust_decimal::Decimal;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

type KlineRequest = (Option<i64>, Option<i64>, u16);

#[derive(Default)]
struct FakeCandles {
    rows: Mutex<BTreeMap<i64, Candle>>,
    operational_sources: Mutex<Vec<ClosedCandleSource>>,
}

impl FakeCandles {
    fn insert(&self, candle: Candle) {
        self.rows.lock().unwrap().insert(candle.open_time, candle);
    }
}

#[async_trait]
impl CandleStore for FakeCandles {
    async fn upsert_page(&self, candles: &[Candle]) -> Result<(), ServiceError> {
        for candle in candles {
            self.insert(candle.clone());
        }
        Ok(())
    }

    async fn persist_page_with_checkpoint(
        &self,
        candles: &[Candle],
        _checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        self.upsert_page(candles).await
    }

    async fn persist_closed_operational(
        &self,
        write: &market_domain::ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        self.operational_sources.lock().unwrap().push(write.source);
        self.insert(write.candle.clone());
        Ok(())
    }

    async fn latest(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Candle>, ServiceError> {
        let mut rows: Vec<_> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter(|candle| before.is_none_or(|value| candle.open_time < value))
            .cloned()
            .collect();
        let start = rows.len().saturating_sub(limit as usize);
        Ok(rows.split_off(start))
    }

    async fn bounds(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .first_key_value()
            .and_then(|(oldest, _)| rows.last_key_value().map(|(latest, _)| (*oldest, *latest))))
    }

    async fn open_times(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .range(start..end)
            .map(|(open_time, _)| *open_time)
            .collect())
    }
}

struct FakeSource {
    returned: Mutex<Vec<Candle>>,
    calls: Mutex<Vec<KlineRequest>>,
}

#[async_trait]
impl MarketSource for FakeSource {
    async fn server_time(&self) -> Result<i64, ServiceError> {
        unreachable!()
    }

    async fn market(&self, _symbol: &Symbol) -> Result<Market, ServiceError> {
        unreachable!()
    }

    async fn klines(
        &self,
        _symbol: &Symbol,
        _timeframe: Timeframe,
        start_time: Option<i64>,
        end_time: Option<i64>,
        limit: u16,
    ) -> Result<Vec<Candle>, ServiceError> {
        self.calls
            .lock()
            .unwrap()
            .push((start_time, end_time, limit));
        Ok(self.returned.lock().unwrap().clone())
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

struct FakeSessions(Vec<MarketSession>);

#[async_trait]
impl SessionStore for FakeSessions {
    async fn upsert_sessions(&self, _sessions: &[MarketSession]) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn covering(
        &self,
        session_market: &str,
        timestamp: i64,
    ) -> Result<Option<MarketSession>, ServiceError> {
        Ok(self
            .0
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
            .0
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
        open_time + 59_999,
        Decimal::new(410000, 2),
        Decimal::new(410200, 2),
        Decimal::new(409800, 2),
        Decimal::new(410050, 2),
    )
    .unwrap()
}

fn sessions() -> Vec<MarketSession> {
    vec![
        MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 120_000, 1).unwrap(),
        MarketSession::new("COMMODITY", SessionStatus::NoTrading, 120_000, 180_000, 1).unwrap(),
        MarketSession::new("COMMODITY", SessionStatus::Regular, 180_000, 300_000, 1).unwrap(),
    ]
}

#[test]
fn expected_higher_timeframe_candle_uses_full_interval_session_policy() {
    let sessions = vec![
        MarketSession::new("COMMODITY", SessionStatus::NoTrading, 0, 30 * 60_000, 1).unwrap(),
        MarketSession::new(
            "COMMODITY",
            SessionStatus::Regular,
            30 * 60_000,
            60 * 60_000,
            1,
        )
        .unwrap(),
    ];

    assert_eq!(
        expected_open_times(Timeframe::H1, 0, Timeframe::H1.duration_ms(), &sessions),
        vec![0]
    );
}

#[test]
fn expected_opens_respect_regular_sessions_and_half_open_boundaries() {
    assert_eq!(
        expected_open_times(Timeframe::M1, 1, 300_000, &sessions()),
        vec![60_000, 180_000, 240_000]
    );
}

#[tokio::test]
async fn repairs_regular_gap_ignores_market_close_and_is_idempotent() {
    let candles = Arc::new(FakeCandles::default());
    for open_time in [0, 180_000, 240_000] {
        candles.insert(candle(open_time));
    }
    let source = Arc::new(FakeSource {
        returned: Mutex::new(vec![candle(60_000)]),
        calls: Mutex::new(Vec::new()),
    });
    let hub = Arc::new(MarketHub::new(16));
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    let reconciler = GapReconciler::new(
        source.clone(),
        candles.clone(),
        Arc::new(FakeSessions(sessions())),
        hub.clone(),
    );

    let report = reconciler
        .reconcile(&market(), Timeframe::M1, 0, 300_000)
        .await
        .unwrap();
    assert_eq!(report.expected, 4);
    assert_eq!(report.already_present, 3);
    assert_eq!(report.repaired, 1);
    assert!(report.unresolved.is_empty());
    assert_eq!(
        candles.operational_sources.lock().unwrap().as_slice(),
        [ClosedCandleSource::DowntimeReconciled]
    );
    assert_eq!(
        hub.snapshot(&symbol(), Timeframe::M1).await.status,
        LiveStatus::Ready
    );

    let second = reconciler
        .reconcile(&market(), Timeframe::M1, 0, 300_000)
        .await
        .unwrap();
    assert_eq!(second.repaired, 0);
    assert_eq!(source.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unresolved_regular_gap_stays_degraded_without_synthetic_candle() {
    let candles = Arc::new(FakeCandles::default());
    for open_time in [0, 180_000, 240_000] {
        candles.insert(candle(open_time));
    }
    let source = Arc::new(FakeSource {
        returned: Mutex::new(Vec::new()),
        calls: Mutex::new(Vec::new()),
    });
    let hub = Arc::new(MarketHub::new(16));
    hub.set_status(&symbol(), LiveStatus::MarketClosed).await;
    let reconciler = GapReconciler::new(
        source,
        candles,
        Arc::new(FakeSessions(sessions())),
        hub.clone(),
    );

    let report = reconciler
        .reconcile(&market(), Timeframe::M1, 0, 300_000)
        .await
        .unwrap();
    assert_eq!(report.unresolved, vec![60_000]);
    assert_eq!(
        hub.snapshot(&symbol(), Timeframe::M1).await.status,
        LiveStatus::Degraded
    );
}
