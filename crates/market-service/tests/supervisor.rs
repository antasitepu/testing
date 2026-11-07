use async_trait::async_trait;
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use market_domain::{
    AggregateTrade, Candle, ClosedCandleSource, LiveStatus, Market, MarketEvent, MarketSession,
    MarketStatus, SessionStatus, Symbol, Timeframe,
};
use market_service::{
    CandleStore, GapReconciler, MarketHub, MarketSource, MarketStreamSource, ServiceError,
    SessionStore, StreamSupervisor, SyncCheckpoint, reconnect_delay,
};
use rust_decimal::Decimal;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

/// The one candle the fake exchange can serve to a reconciliation request.
const REPAIRABLE_OPEN_TIME: i64 = 60_000;

struct StreamScript {
    events: Vec<Result<MarketEvent, ServiceError>>,
    stay_open: bool,
}

type SupervisorHarness = (
    Arc<FakeStream>,
    Arc<FakeCandles>,
    Arc<FakeSessions>,
    Arc<MarketHub>,
    Arc<StreamSupervisor>,
);

struct FakeStream {
    scripts: Mutex<VecDeque<StreamScript>>,
    connections: AtomicUsize,
}

#[async_trait]
impl MarketStreamSource for FakeStream {
    async fn connect(
        &self,
        _symbol: &Symbol,
    ) -> Result<BoxStream<'static, Result<MarketEvent, ServiceError>>, ServiceError> {
        self.connections.fetch_add(1, Ordering::SeqCst);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(StreamScript {
                events: Vec::new(),
                stay_open: true,
            });
        let finite = stream::iter(script.events);
        if script.stay_open {
            Ok(Box::pin(finite.chain(stream::pending())))
        } else {
            Ok(Box::pin(finite))
        }
    }
}

#[derive(Default)]
struct FakeCandles {
    persisted: Mutex<Vec<(i64, ClosedCandleSource)>>,
    fail_next_persist: AtomicBool,
    m1_bounds: Mutex<Option<(i64, i64)>>,
}

#[async_trait]
impl CandleStore for FakeCandles {
    async fn upsert_page(&self, _candles: &[Candle]) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn persist_page_with_checkpoint(
        &self,
        _candles: &[Candle],
        _checkpoint: &SyncCheckpoint,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn persist_closed_operational(
        &self,
        write: &market_domain::ClosedCandleWrite,
    ) -> Result<(), ServiceError> {
        if self.fail_next_persist.swap(false, Ordering::SeqCst) {
            return Err(ServiceError::Storage {
                operation: "persist closed candle",
                message: "connection reset".to_owned(),
            });
        }
        self.persisted
            .lock()
            .unwrap()
            .push((write.candle.open_time, write.source));
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
        timeframe: Timeframe,
    ) -> Result<Option<(i64, i64)>, ServiceError> {
        if timeframe == Timeframe::M1 {
            return Ok(*self.m1_bounds.lock().unwrap());
        }
        Ok(None)
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

#[derive(Default)]
struct FakeSessions {
    persisted: Mutex<Vec<MarketSession>>,
    known: Mutex<Vec<MarketSession>>,
}

#[async_trait]
impl SessionStore for FakeSessions {
    async fn upsert_sessions(&self, sessions: &[MarketSession]) -> Result<(), ServiceError> {
        self.persisted.lock().unwrap().extend_from_slice(sessions);
        Ok(())
    }

    async fn covering(
        &self,
        _session_market: &str,
        _timestamp: i64,
    ) -> Result<Option<MarketSession>, ServiceError> {
        Ok(None)
    }

    async fn between(
        &self,
        _session_market: &str,
        _start: i64,
        _end: i64,
    ) -> Result<Vec<MarketSession>, ServiceError> {
        Ok(self.known.lock().unwrap().clone())
    }
}

struct FakeRest;

#[async_trait]
impl MarketSource for FakeRest {
    async fn server_time(&self) -> Result<i64, ServiceError> {
        Ok(300_000)
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
        _symbol: &Symbol,
        timeframe: Timeframe,
        start_time: Option<i64>,
        end_time: Option<i64>,
        _limit: u16,
    ) -> Result<Vec<Candle>, ServiceError> {
        let in_range = start_time.is_none_or(|start| start <= REPAIRABLE_OPEN_TIME)
            && end_time.is_none_or(|end| REPAIRABLE_OPEN_TIME <= end);
        if timeframe == Timeframe::M1 && in_range {
            return Ok(vec![m1_candle(REPAIRABLE_OPEN_TIME)]);
        }
        Ok(Vec::new())
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

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn m1_candle(open_time: i64) -> Candle {
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

fn candle(open_time: i64, closed: bool) -> MarketEvent {
    let candle = m1_candle(open_time);
    if closed {
        MarketEvent::ClosedCandle(candle)
    } else {
        MarketEvent::FormingCandle(candle)
    }
}

fn supervisor(scripts: Vec<StreamScript>) -> SupervisorHarness {
    let websocket = Arc::new(FakeStream {
        scripts: Mutex::new(scripts.into()),
        connections: AtomicUsize::new(0),
    });
    let candles = Arc::new(FakeCandles::default());
    let sessions = Arc::new(FakeSessions::default());
    let hub = Arc::new(MarketHub::new(32));
    let reconciler = Arc::new(GapReconciler::new(
        Arc::new(FakeRest),
        candles.clone(),
        sessions.clone(),
        hub.clone(),
    ));
    let supervisor = Arc::new(StreamSupervisor::new(
        websocket.clone(),
        candles.clone(),
        sessions.clone(),
        hub.clone(),
        reconciler,
    ));
    (websocket, candles, sessions, hub, supervisor)
}

#[tokio::test]
async fn closed_candle_and_session_are_persisted_before_publication() {
    let session =
        MarketSession::new("COMMODITY", SessionStatus::NoTrading, 0, 300_000, 10).unwrap();
    let (_stream, candles, sessions, hub, supervisor) = supervisor(vec![StreamScript {
        events: vec![
            Ok(candle(60_000, true)),
            Ok(MarketEvent::SessionChanged(session)),
        ],
        stay_open: true,
    }]);
    let mut subscriber = hub.subscribe();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { supervisor.run(symbol(), shutdown_rx).await });

    let mut saw_closed = false;
    let mut saw_session = false;
    while !(saw_closed && saw_session) {
        match subscriber.recv().await.unwrap() {
            MarketEvent::ClosedCandle(_) => {
                assert_eq!(
                    candles.persisted.lock().unwrap().as_slice(),
                    [(60_000, ClosedCandleSource::Live)]
                );
                saw_closed = true;
            }
            MarketEvent::SessionChanged(_) => {
                assert_eq!(sessions.persisted.lock().unwrap().len(), 1);
                saw_session = true;
            }
            _ => {}
        }
    }
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn disconnect_degrades_then_reconnects_and_cancellation_stops_loop() {
    let (stream, _candles, _sessions, hub, supervisor) = supervisor(vec![
        StreamScript {
            events: Vec::new(),
            stay_open: false,
        },
        StreamScript {
            events: Vec::new(),
            stay_open: true,
        },
    ]);
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    let mut subscriber = hub.subscribe();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { supervisor.run(symbol(), shutdown_rx).await });

    let mut degraded = false;
    while !degraded {
        degraded = matches!(
            subscriber.recv().await.unwrap(),
            MarketEvent::StatusChanged {
                status: LiveStatus::Degraded,
                ..
            }
        );
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(stream.connections.load(Ordering::SeqCst), 2);
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn storage_failure_reconnects_instead_of_stopping_ingestion() {
    let (stream, candles, _sessions, _hub, supervisor) = supervisor(vec![
        StreamScript {
            events: vec![Ok(candle(60_000, true))],
            stay_open: true,
        },
        StreamScript {
            events: vec![Ok(candle(120_000, true))],
            stay_open: true,
        },
    ]);
    candles.fail_next_persist.store(true, Ordering::SeqCst);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { supervisor.run(symbol(), shutdown_rx).await });

    for _ in 0..100 {
        if !candles.persisted.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        candles.persisted.lock().unwrap().as_slice(),
        [(120_000, ClosedCandleSource::Live)]
    );
    assert_eq!(stream.connections.load(Ordering::SeqCst), 2);
    assert!(!task.is_finished());
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn disconnect_with_unresolved_gap_keeps_status_degraded() {
    let (_stream, candles, sessions, hub, supervisor) = supervisor(vec![
        StreamScript {
            events: Vec::new(),
            stay_open: false,
        },
        StreamScript {
            events: Vec::new(),
            stay_open: true,
        },
    ]);
    *candles.m1_bounds.lock().unwrap() = Some((0, 0));
    sessions
        .known
        .lock()
        .unwrap()
        .push(MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 300_000, 10).unwrap());
    hub.set_status(&symbol(), LiveStatus::Ready).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { supervisor.run(symbol(), shutdown_rx).await });

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hub.snapshot(&symbol(), Timeframe::M1).await.status,
        LiveStatus::Degraded
    );
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn candle_closed_before_stream_connected_is_repaired_without_a_disconnect() {
    let (stream, candles, sessions, _hub, supervisor) = supervisor(vec![StreamScript {
        events: Vec::new(),
        stay_open: true,
    }]);
    *candles.m1_bounds.lock().unwrap() = Some((0, 0));
    sessions
        .known
        .lock()
        .unwrap()
        .push(MarketSession::new("COMMODITY", SessionStatus::Regular, 0, 300_000, 10).unwrap());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move { supervisor.run(symbol(), shutdown_rx).await });

    for _ in 0..100 {
        if !candles.persisted.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        candles.persisted.lock().unwrap().as_slice(),
        [(REPAIRABLE_OPEN_TIME, ClosedCandleSource::DowntimeReconciled)]
    );
    assert_eq!(stream.connections.load(Ordering::SeqCst), 1);
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn reconnect_backoff_is_capped() {
    let delays: Vec<_> = (0..7).map(reconnect_delay).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30].map(Duration::from_secs));
}
