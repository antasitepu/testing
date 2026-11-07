use crate::{MarketHub, MarketSource, MarketStore, RecentSync, ServiceError, SessionStore};
use market_domain::{LiveStatus, Market, SessionStatus, Symbol, Timeframe};
use std::sync::Arc;

pub struct StartupCoordinator {
    source: Arc<dyn MarketSource>,
    markets: Arc<dyn MarketStore>,
    sessions: Arc<dyn SessionStore>,
    recent_sync: Arc<RecentSync>,
    hub: Arc<MarketHub>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupOutcome {
    pub market: Market,
    pub final_status: LiveStatus,
    pub backfill_timeframes: Vec<Timeframe>,
}

impl StartupCoordinator {
    pub fn new(
        source: Arc<dyn MarketSource>,
        markets: Arc<dyn MarketStore>,
        sessions: Arc<dyn SessionStore>,
        recent_sync: Arc<RecentSync>,
        hub: Arc<MarketHub>,
    ) -> Self {
        Self {
            source,
            markets,
            sessions,
            recent_sync,
            hub,
        }
    }

    pub async fn initialize(&self, symbol: Symbol) -> Result<StartupOutcome, ServiceError> {
        self.hub.set_status(&symbol, LiveStatus::Syncing).await;
        let server_time = self.source.server_time().await?;
        let market = self.source.market(&symbol).await?;
        self.markets.upsert_market(&market).await?;
        let sessions = self.source.trading_schedule(&market.session_market).await?;
        self.sessions.upsert_sessions(&sessions).await?;

        // Higher timeframes first so catch-up work for lower timeframes sees the
        // higher-timeframe context at the same boundary.
        for timeframe in Timeframe::ALL.into_iter().rev() {
            self.recent_sync
                .run(&symbol, timeframe, server_time)
                .await?;
        }

        let current_session = sessions
            .into_iter()
            .filter(|session| session.contains(server_time))
            .max_by_key(|session| session.source_event_time);
        let final_status = match current_session.as_ref().map(|session| session.status) {
            Some(SessionStatus::NoTrading) => LiveStatus::MarketClosed,
            _ => LiveStatus::Ready,
        };
        if let Some(session) = current_session {
            self.hub.apply_session(&symbol, session).await;
        } else {
            self.hub.set_status(&symbol, final_status).await;
        }

        Ok(StartupOutcome {
            market,
            final_status,
            backfill_timeframes: Timeframe::ALL.to_vec(),
        })
    }
}
