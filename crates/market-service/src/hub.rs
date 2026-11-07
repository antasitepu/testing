use market_domain::{
    AggregateTrade, Candle, LiveStatus, MarketEvent, MarketSession, SessionStatus, Symbol,
    Timeframe,
};
use runtime_observability::record_market_resync;
use std::collections::HashMap;
use tokio::sync::{RwLock, broadcast};

pub struct MarketHub {
    forming: RwLock<HashMap<(Symbol, Timeframe), Candle>>,
    latest_trade: RwLock<HashMap<Symbol, AggregateTrade>>,
    sessions: RwLock<HashMap<String, MarketSession>>,
    statuses: RwLock<HashMap<Symbol, LiveStatus>>,
    sender: broadcast::Sender<MarketEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketSnapshot {
    pub forming: Option<Candle>,
    pub latest_trade: Option<AggregateTrade>,
    pub status: LiveStatus,
}

impl MarketHub {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            forming: RwLock::new(HashMap::new()),
            latest_trade: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            statuses: RwLock::new(HashMap::new()),
            sender,
        }
    }

    pub async fn apply_forming(&self, candle: Candle) {
        self.forming
            .write()
            .await
            .insert((candle.symbol.clone(), candle.timeframe), candle.clone());
        self.publish(MarketEvent::FormingCandle(candle));
    }

    pub async fn apply_closed(&self, candle: Candle) {
        let key = (candle.symbol.clone(), candle.timeframe);
        let mut forming = self.forming.write().await;
        if forming
            .get(&key)
            .is_some_and(|current| current.open_time == candle.open_time)
        {
            forming.remove(&key);
        }
        drop(forming);
        self.publish(MarketEvent::ClosedCandle(candle));
    }

    pub async fn apply_trade(&self, trade: AggregateTrade) {
        let symbol = trade.symbol.clone();
        let mut resync_reason = None;
        let mut latest = self.latest_trade.write().await;
        match latest.get(&symbol) {
            Some(previous) if trade.aggregate_trade_id <= previous.aggregate_trade_id => return,
            Some(previous) if trade.aggregate_trade_id != previous.aggregate_trade_id + 1 => {
                resync_reason = Some(format!(
                    "aggregate trade sequence jumped from {} to {}",
                    previous.aggregate_trade_id, trade.aggregate_trade_id
                ));
                latest.insert(symbol.clone(), trade.clone());
            }
            _ => {
                latest.insert(symbol.clone(), trade.clone());
            }
        }
        drop(latest);
        if let Some(reason) = resync_reason {
            record_market_resync();
            self.set_status(&symbol, LiveStatus::Degraded).await;
            self.publish(MarketEvent::ResyncRequired {
                symbol: symbol.clone(),
                reason,
            });
        }
        self.publish(MarketEvent::AggregateTrade(trade));
    }

    pub async fn apply_session(&self, symbol: &Symbol, session: MarketSession) {
        self.sessions
            .write()
            .await
            .insert(session.session_market.clone(), session.clone());
        self.publish(MarketEvent::SessionChanged(session.clone()));
        let status = match session.status {
            SessionStatus::Regular => LiveStatus::Ready,
            SessionStatus::NoTrading => LiveStatus::MarketClosed,
        };
        self.set_status(symbol, status).await;
    }

    pub async fn set_status(&self, symbol: &Symbol, status: LiveStatus) {
        self.statuses.write().await.insert(symbol.clone(), status);
        self.publish(MarketEvent::StatusChanged {
            symbol: symbol.clone(),
            status,
        });
    }

    pub async fn snapshot(&self, symbol: &Symbol, timeframe: Timeframe) -> MarketSnapshot {
        let forming = self
            .forming
            .read()
            .await
            .get(&(symbol.clone(), timeframe))
            .cloned();
        let latest_trade = self.latest_trade.read().await.get(symbol).cloned();
        let status = self
            .statuses
            .read()
            .await
            .get(symbol)
            .copied()
            .unwrap_or(LiveStatus::Syncing);
        MarketSnapshot {
            forming,
            latest_trade,
            status,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MarketEvent> {
        self.sender.subscribe()
    }

    fn publish(&self, event: MarketEvent) {
        let _ = self.sender.send(event);
    }
}
