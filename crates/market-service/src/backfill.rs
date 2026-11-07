use crate::{CandleStore, MarketSource, ServiceError, SyncCheckpoint, SyncOperation, SyncStore};
use market_domain::{Market, Symbol, Timeframe};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;

pub struct HistoricalBackfill {
    source: Arc<dyn MarketSource>,
    candles: Arc<dyn CandleStore>,
    sync: Arc<dyn SyncStore>,
    page_limit: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackfillStep {
    Progress { oldest_open_time: i64, rows: usize },
    Complete,
}

impl HistoricalBackfill {
    pub fn new(
        source: Arc<dyn MarketSource>,
        candles: Arc<dyn CandleStore>,
        sync: Arc<dyn SyncStore>,
        page_limit: u16,
    ) -> Self {
        Self {
            source,
            candles,
            sync,
            page_limit,
        }
    }

    pub async fn step(
        &self,
        market: &Market,
        timeframe: Timeframe,
    ) -> Result<BackfillStep, ServiceError> {
        let status = self.sync.load(&market.symbol, timeframe).await?;
        if status
            .as_ref()
            .is_some_and(|state| state.full_history_complete)
        {
            return Ok(BackfillStep::Complete);
        }
        let bounds = self.candles.bounds(&market.symbol, timeframe).await?;
        let oldest = status
            .as_ref()
            .and_then(|state| state.oldest_synced_time)
            .or_else(|| bounds.map(|value| value.0))
            .ok_or_else(|| {
                ServiceError::InvalidData(format!(
                    "cannot backfill {} {timeframe} without a recent-sync checkpoint",
                    market.symbol
                ))
            })?;
        let latest = status
            .as_ref()
            .and_then(|state| state.latest_synced_time)
            .or_else(|| bounds.map(|value| value.1))
            .unwrap_or(oldest);
        if oldest <= market.onboard_time {
            self.sync
                .mark_backfill_complete(&market.symbol, timeframe)
                .await?;
            return Ok(BackfillStep::Complete);
        }

        self.sync
            .mark_backfill_running(&market.symbol, timeframe)
            .await?;
        let end_time = oldest
            .checked_sub(1)
            .ok_or_else(|| ServiceError::InvalidData("backfill cursor underflowed".to_owned()))?;
        let fetched = match self
            .source
            .klines(
                &market.symbol,
                timeframe,
                None,
                Some(end_time),
                self.page_limit,
            )
            .await
        {
            Ok(page) => page,
            Err(error) => {
                self.record_failure(&market.symbol, timeframe, &error).await;
                return Err(error);
            }
        };
        if fetched.is_empty() {
            self.sync
                .mark_backfill_complete(&market.symbol, timeframe)
                .await?;
            return Ok(BackfillStep::Complete);
        }

        let reached_onboard = fetched
            .iter()
            .map(|candle| candle.open_time)
            .min()
            .is_some_and(|value| value <= market.onboard_time);
        let mut page: Vec<_> = fetched
            .into_iter()
            .filter(|candle| {
                candle.symbol == market.symbol
                    && candle.timeframe == timeframe
                    && candle.open_time < oldest
                    && candle.open_time >= market.onboard_time
            })
            .collect();
        page.sort_by_key(|candle| candle.open_time);
        if page.is_empty() {
            if reached_onboard {
                self.sync
                    .mark_backfill_complete(&market.symbol, timeframe)
                    .await?;
                return Ok(BackfillStep::Complete);
            }
            let error = ServiceError::InvalidData(format!(
                "backfill page did not advance backward for {} {timeframe}",
                market.symbol
            ));
            self.record_failure(&market.symbol, timeframe, &error).await;
            return Err(error);
        }
        let page_oldest = page.first().expect("validated non-empty page").open_time;
        let checkpoint = SyncCheckpoint {
            symbol: market.symbol.clone(),
            timeframe,
            oldest_synced_time: page_oldest,
            latest_synced_time: latest,
            operation: SyncOperation::Backfill,
            complete: reached_onboard || page_oldest <= market.onboard_time,
        };
        self.candles
            .persist_page_with_checkpoint(&page, &checkpoint)
            .await?;
        self.sync
            .mark_backfill_progress(&market.symbol, timeframe, page_oldest)
            .await?;

        if checkpoint.complete {
            self.sync
                .mark_backfill_complete(&market.symbol, timeframe)
                .await?;
            Ok(BackfillStep::Complete)
        } else {
            Ok(BackfillStep::Progress {
                oldest_open_time: page_oldest,
                rows: page.len(),
            })
        }
    }

    pub async fn run_round_robin(
        &self,
        market: Market,
        timeframes: Vec<Timeframe>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), ServiceError> {
        let mut queue: VecDeque<Timeframe> = timeframes.into();
        let mut failures: HashMap<Timeframe, u32> = HashMap::new();
        while let Some(timeframe) = queue.pop_front() {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.step(&market, timeframe).await {
                Ok(BackfillStep::Complete) => {
                    failures.remove(&timeframe);
                }
                Ok(BackfillStep::Progress { .. }) => {
                    failures.remove(&timeframe);
                    queue.push_back(timeframe);
                    if sleep_or_shutdown(Duration::from_millis(250), &mut shutdown).await {
                        return Ok(());
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        symbol = %market.symbol,
                        timeframe = %timeframe,
                        error = %error,
                        "historical backfill page failed"
                    );
                    let attempt = failures.entry(timeframe).or_default();
                    let delay = backoff_delay(*attempt);
                    *attempt = attempt.saturating_add(1);
                    queue.push_back(timeframe);
                    if sleep_or_shutdown(delay, &mut shutdown).await {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    async fn record_failure(&self, symbol: &Symbol, timeframe: Timeframe, error: &ServiceError) {
        let _ = self
            .sync
            .mark_failed(
                symbol,
                timeframe,
                SyncOperation::Backfill,
                &error.to_string(),
            )
            .await;
    }
}

fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_secs(2_u64.saturating_pow(attempt.min(5)).min(30))
}

async fn sleep_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => false,
        result = shutdown.changed() => result.is_err() || *shutdown.borrow(),
    }
}
