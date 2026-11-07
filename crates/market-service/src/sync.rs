use crate::{CandleStore, MarketSource, ServiceError, SyncCheckpoint, SyncOperation, SyncStore};
use market_domain::{Candle, ClosedCandleSource, ClosedCandleWrite, Symbol, Timeframe};
use std::sync::Arc;

pub struct RecentSync {
    source: Arc<dyn MarketSource>,
    candles: Arc<dyn CandleStore>,
    sync: Arc<dyn SyncStore>,
    initial_limit: u16,
    page_limit: u16,
}

impl RecentSync {
    pub fn new(
        source: Arc<dyn MarketSource>,
        candles: Arc<dyn CandleStore>,
        sync: Arc<dyn SyncStore>,
        initial_limit: u16,
        page_limit: u16,
    ) -> Self {
        Self {
            source,
            candles,
            sync,
            initial_limit,
            page_limit,
        }
    }

    pub async fn run(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        server_time: i64,
    ) -> Result<(), ServiceError> {
        self.sync.mark_recent_running(symbol, timeframe).await?;
        let initial_bounds = self.candles.bounds(symbol, timeframe).await?;
        let mut oldest = initial_bounds.map(|bounds| bounds.0);
        let mut latest = initial_bounds.map(|bounds| bounds.1);
        let mut start_time = latest.map(|open_time| open_time + timeframe.duration_ms());
        let mut limit = if start_time.is_some() {
            self.page_limit
        } else {
            self.initial_limit
        };

        loop {
            let fetched = match self
                .source
                .klines(symbol, timeframe, start_time, Some(server_time), limit)
                .await
            {
                Ok(candles) => candles,
                Err(error) => {
                    let _ = self
                        .sync
                        .mark_failed(symbol, timeframe, SyncOperation::Recent, &error.to_string())
                        .await;
                    return Err(error);
                }
            };
            let fetched_count = fetched.len();
            let closed: Vec<Candle> = fetched
                .into_iter()
                .filter(|candle| candle.is_closed_at(server_time))
                .collect();

            if closed.is_empty() {
                if let (Some(oldest), Some(latest)) = (oldest, latest) {
                    self.sync
                        .mark_recent_complete(symbol, timeframe, oldest, latest)
                        .await?;
                    return Ok(());
                }
                let error = ServiceError::InvalidData(format!(
                    "no closed candles available for {symbol} {timeframe}"
                ));
                self.sync
                    .mark_failed(symbol, timeframe, SyncOperation::Recent, &error.to_string())
                    .await?;
                return Err(error);
            }

            let page_oldest = closed
                .iter()
                .map(|candle| candle.open_time)
                .min()
                .expect("closed page is non-empty");
            let page_latest = closed
                .iter()
                .map(|candle| candle.open_time)
                .max()
                .expect("closed page is non-empty");
            oldest = Some(oldest.map_or(page_oldest, |value| value.min(page_oldest)));
            latest = Some(latest.map_or(page_latest, |value| value.max(page_latest)));
            let complete = fetched_count < usize::from(limit);
            let checkpoint = SyncCheckpoint {
                symbol: symbol.clone(),
                timeframe,
                oldest_synced_time: oldest.expect("oldest is set"),
                latest_synced_time: latest.expect("latest is set"),
                operation: SyncOperation::Recent,
                complete,
            };
            let writes: Vec<_> = closed
                .into_iter()
                .map(|candle| ClosedCandleWrite {
                    candle,
                    source: ClosedCandleSource::CatchUp,
                })
                .collect();
            self.candles
                .persist_closed_page_with_checkpoint(&writes, &checkpoint)
                .await?;

            if complete {
                self.sync
                    .mark_recent_complete(
                        symbol,
                        timeframe,
                        checkpoint.oldest_synced_time,
                        checkpoint.latest_synced_time,
                    )
                    .await?;
                return Ok(());
            }
            start_time = Some(page_latest + timeframe.duration_ms());
            limit = self.page_limit;
        }
    }
}
