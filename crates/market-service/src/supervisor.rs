use crate::{
    CandleStore, GapReconciler, MarketHub, MarketStreamSource, ReconciliationReport, ServiceError,
    SessionStore,
};
use futures_util::StreamExt;
use market_domain::{
    ClosedCandleSource, ClosedCandleWrite, LiveStatus, MarketEvent, Symbol, Timeframe,
};
use runtime_observability::{
    ActivityKind, record_activity, record_market_reconnect, record_market_resync,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

pub struct StreamSupervisor {
    websocket: Arc<dyn MarketStreamSource>,
    candles: Arc<dyn CandleStore>,
    sessions: Arc<dyn SessionStore>,
    hub: Arc<MarketHub>,
    reconciler: Arc<GapReconciler>,
}

impl StreamSupervisor {
    pub fn new(
        websocket: Arc<dyn MarketStreamSource>,
        candles: Arc<dyn CandleStore>,
        sessions: Arc<dyn SessionStore>,
        hub: Arc<MarketHub>,
        reconciler: Arc<GapReconciler>,
    ) -> Self {
        Self {
            websocket,
            candles,
            sessions,
            hub,
            reconciler,
        }
    }

    pub async fn run(
        &self,
        symbol: Symbol,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), ServiceError> {
        let mut reconnect_attempt = 0;
        let mut restore_status = None;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let mut stream = match self.websocket.connect(&symbol).await {
                Ok(stream) => stream,
                Err(error) => {
                    record_market_reconnect();
                    tracing::warn!(symbol = %symbol, error = %error, "websocket connect failed");
                    self.handle_disconnect(&symbol, &mut restore_status).await;
                    if wait_or_shutdown(reconnect_delay(reconnect_attempt), &mut shutdown).await {
                        return Ok(());
                    }
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    continue;
                }
            };
            self.reconcile_connected(&symbol, &mut restore_status).await;

            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    item = stream.next() => {
                        match item {
                            Some(Ok(event)) => {
                                // A storage failure must not end ingestion: drop the
                                // stream and let disconnect reconciliation repair the
                                // closed candles that were not persisted.
                                if let Err(error) = self.process_event(&symbol, event, &mut restore_status).await {
                                    tracing::warn!(symbol = %symbol, error = %error, "market event processing failed");
                                    break;
                                }
                                reconnect_attempt = 0;
                            }
                            Some(Err(error)) => {
                                tracing::warn!(symbol = %symbol, error = %error, "websocket stream failed");
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }

            record_market_reconnect();
            self.handle_disconnect(&symbol, &mut restore_status).await;
            if wait_or_shutdown(reconnect_delay(reconnect_attempt), &mut shutdown).await {
                return Ok(());
            }
            reconnect_attempt = reconnect_attempt.saturating_add(1);
        }
    }

    async fn process_event(
        &self,
        symbol: &Symbol,
        event: MarketEvent,
        restore: &mut Option<LiveStatus>,
    ) -> Result<(), ServiceError> {
        match event {
            MarketEvent::FormingCandle(candle) => {
                self.hub.apply_forming(candle).await;
            }
            MarketEvent::ClosedCandle(candle) => {
                self.candles
                    .persist_closed_operational(&ClosedCandleWrite {
                        candle: candle.clone(),
                        source: ClosedCandleSource::Live,
                    })
                    .await?;
                self.hub.apply_closed(candle).await;
            }
            MarketEvent::AggregateTrade(trade) => {
                let snapshot = self.hub.snapshot(&trade.symbol, Timeframe::M1).await;
                let sequence_gap = snapshot.latest_trade.is_some_and(|previous| {
                    trade
                        .aggregate_trade_id
                        .saturating_sub(previous.aggregate_trade_id)
                        > 1
                });
                if sequence_gap
                    && matches!(
                        snapshot.status,
                        LiveStatus::Ready | LiveStatus::MarketClosed
                    )
                {
                    *restore = Some(snapshot.status);
                }
                self.hub.apply_trade(trade).await;
                if sequence_gap {
                    self.reconcile_connected(symbol, restore).await;
                }
            }
            MarketEvent::SessionChanged(session) => {
                self.sessions
                    .upsert_sessions(std::slice::from_ref(&session))
                    .await?;
                self.hub.apply_session(symbol, session).await;
            }
            MarketEvent::StatusChanged { status, .. } => {
                self.hub.set_status(symbol, status).await;
            }
            MarketEvent::ResyncRequired { .. } => {
                record_market_resync();
                self.mark_disconnected(symbol, restore).await;
                self.reconcile_connected(symbol, restore).await;
            }
        }
        record_activity(ActivityKind::Market, current_unix_millis());
        Ok(())
    }

    async fn mark_disconnected(&self, symbol: &Symbol, restore: &mut Option<LiveStatus>) {
        let status = self.hub.snapshot(symbol, Timeframe::M1).await.status;
        if matches!(status, LiveStatus::Ready | LiveStatus::MarketClosed) {
            *restore = Some(status);
        }
        self.hub.set_status(symbol, LiveStatus::Degraded).await;
    }

    async fn handle_disconnect(&self, symbol: &Symbol, restore: &mut Option<LiveStatus>) {
        self.mark_disconnected(symbol, restore).await;
        // REST can repair candles during a socket outage, but only a connected feed can be ready.
        self.reconcile(symbol).await;
    }

    async fn reconcile_connected(&self, symbol: &Symbol, restore: &mut Option<LiveStatus>) {
        let status = self.hub.snapshot(symbol, Timeframe::M1).await.status;
        if self.reconcile(symbol).await {
            if let Some(previous) = restore.take() {
                self.hub.set_status(symbol, previous).await;
            }
        } else {
            if matches!(status, LiveStatus::Ready | LiveStatus::MarketClosed) {
                *restore = Some(status);
            }
            self.hub.set_status(symbol, LiveStatus::Degraded).await;
        }
    }

    async fn reconcile(&self, symbol: &Symbol) -> bool {
        match self.reconciler.reconcile_to_server_time(symbol).await {
            Ok(reports) => {
                log_unresolved(symbol, &reports);
                fully_resolved(&reports)
            }
            Err(error) => {
                tracing::warn!(%symbol, %error, "market reconciliation failed");
                false
            }
        }
    }
}

fn fully_resolved(reports: &[(Timeframe, ReconciliationReport)]) -> bool {
    reports
        .iter()
        .all(|(_, report)| report.unresolved.is_empty())
}

fn log_unresolved(symbol: &Symbol, reports: &[(Timeframe, ReconciliationReport)]) {
    for (timeframe, report) in reports {
        if !report.unresolved.is_empty() {
            tracing::warn!(
                symbol = %symbol,
                timeframe = timeframe.as_str(),
                unresolved = report.unresolved.len(),
                "reconciliation left unresolved candles; status stays degraded"
            );
        }
    }
}

fn current_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs(2_u64.saturating_pow(attempt.min(5)).min(30))
}

async fn wait_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => false,
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
    }
}
