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
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let mut stream = match self.websocket.connect(&symbol).await {
                Ok(stream) => stream,
                Err(error) => {
                    record_market_reconnect();
                    tracing::warn!(symbol = %symbol, error = %error, "websocket connect failed");
                    self.handle_disconnect(&symbol).await;
                    if wait_or_shutdown(reconnect_delay(reconnect_attempt), &mut shutdown).await {
                        return Ok(());
                    }
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    continue;
                }
            };
            self.reconcile_after_connect(&symbol).await;

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
                                if let Err(error) = self.process_event(&symbol, event).await {
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
            self.handle_disconnect(&symbol).await;
            if wait_or_shutdown(reconnect_delay(reconnect_attempt), &mut shutdown).await {
                return Ok(());
            }
            reconnect_attempt = reconnect_attempt.saturating_add(1);
        }
    }

    async fn process_event(&self, symbol: &Symbol, event: MarketEvent) -> Result<(), ServiceError> {
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
                let previous = self
                    .hub
                    .snapshot(&trade.symbol, Timeframe::M1)
                    .await
                    .latest_trade;
                let prior_status = self.hub.snapshot(&trade.symbol, Timeframe::M1).await.status;
                let sequence_gap = previous.as_ref().is_some_and(|previous| {
                    trade.aggregate_trade_id > previous.aggregate_trade_id + 1
                });
                self.hub.apply_trade(trade).await;
                if sequence_gap {
                    // Match disconnect semantics: degrade until reconcile succeeds.
                    self.hub.set_status(symbol, LiveStatus::Degraded).await;
                    match self.reconciler.reconcile_to_server_time(symbol).await {
                        Ok(reports)
                            if fully_resolved(&reports)
                                && matches!(
                                    prior_status,
                                    LiveStatus::Ready | LiveStatus::MarketClosed
                                ) =>
                        {
                            self.hub.set_status(symbol, prior_status).await;
                        }
                        Ok(reports) => log_unresolved(symbol, &reports),
                        Err(error) => {
                            tracing::warn!(
                                symbol = %symbol,
                                error = %error,
                                "sequence gap reconciliation failed"
                            );
                        }
                    }
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
                self.handle_disconnect(symbol).await;
            }
        }
        record_activity(ActivityKind::Market, current_unix_millis());
        Ok(())
    }

    /// A candle that closed before this subscription went live produces no stream
    /// event, so repair anything missed since the previous sync.
    async fn reconcile_after_connect(&self, symbol: &Symbol) {
        match self.reconciler.reconcile_to_server_time(symbol).await {
            Ok(reports) => log_unresolved(symbol, &reports),
            Err(error) => {
                tracing::warn!(symbol = %symbol, error = %error, "post-connect reconciliation failed");
            }
        }
    }

    async fn handle_disconnect(&self, symbol: &Symbol) {
        let prior_status = self.hub.snapshot(symbol, Timeframe::M1).await.status;
        self.hub.set_status(symbol, LiveStatus::Degraded).await;
        match self.reconciler.reconcile_to_server_time(symbol).await {
            Ok(reports)
                if fully_resolved(&reports)
                    && matches!(prior_status, LiveStatus::Ready | LiveStatus::MarketClosed) =>
            {
                self.hub.set_status(symbol, prior_status).await;
            }
            Ok(reports) => log_unresolved(symbol, &reports),
            Err(error) => {
                tracing::warn!(symbol = %symbol, error = %error, "disconnect reconciliation failed");
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
