use crate::{
    CandleSessionClass, CandleStore, MarketHub, MarketSource, ServiceError, SessionStore,
    TradableCandlePolicy,
};
use market_domain::{
    ClosedCandleSource, ClosedCandleWrite, LiveStatus, Market, MarketSession, Timeframe,
};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

pub fn expected_open_times(
    timeframe: Timeframe,
    start: i64,
    end: i64,
    sessions: &[MarketSession],
) -> Vec<i64> {
    if end <= start {
        return Vec::new();
    }
    let duration = timeframe.duration_ms();
    let remainder = start.rem_euclid(duration);
    let Some(mut open_time) = (if remainder == 0 {
        Some(start)
    } else {
        start.checked_add(duration - remainder)
    }) else {
        return Vec::new();
    };
    let mut expected = BTreeSet::new();
    while let Some(candle_end) = open_time.checked_add(duration) {
        if candle_end > end {
            break;
        }
        if TradableCandlePolicy.classify_range(open_time, candle_end, sessions)
            == CandleSessionClass::Tradable
        {
            expected.insert(open_time);
        }
        open_time = candle_end;
    }
    expected.into_iter().collect()
}

/// Holes behind the newest stored candle are re-checked on every reconciliation
/// (same 200-bar window the analysis gap gate inspects), so a repair that failed
/// once is retried instead of being left behind forever.
const RECONCILE_LOOKBACK_BARS: i64 = 200;

fn reconcile_start(timeframe: Timeframe, oldest: i64, latest: i64) -> i64 {
    timeframe
        .duration_ms()
        .checked_mul(RECONCILE_LOOKBACK_BARS)
        .map_or(oldest, |lookback| latest.saturating_sub(lookback))
        .max(oldest)
}

pub struct GapReconciler {
    source: Arc<dyn MarketSource>,
    candles: Arc<dyn CandleStore>,
    sessions: Arc<dyn SessionStore>,
    hub: Arc<MarketHub>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationReport {
    pub expected: usize,
    pub already_present: usize,
    pub repaired: usize,
    pub unresolved: Vec<i64>,
}

impl GapReconciler {
    pub fn new(
        source: Arc<dyn MarketSource>,
        candles: Arc<dyn CandleStore>,
        sessions: Arc<dyn SessionStore>,
        hub: Arc<MarketHub>,
    ) -> Self {
        Self {
            source,
            candles,
            sessions,
            hub,
        }
    }

    pub async fn reconcile(
        &self,
        market: &Market,
        timeframe: Timeframe,
        start: i64,
        end: i64,
    ) -> Result<ReconciliationReport, ServiceError> {
        if end <= start {
            return Err(ServiceError::InvalidData(
                "reconciliation end must be later than start".to_owned(),
            ));
        }
        let sessions = self
            .sessions
            .between(&market.session_market, start, end)
            .await?;
        let expected = expected_open_times(timeframe, start, end, &sessions);
        let expected_set: HashSet<i64> = expected.iter().copied().collect();
        let initial_present: HashSet<i64> = self
            .candles
            .open_times(&market.symbol, timeframe, start, end)
            .await?
            .into_iter()
            .filter(|open_time| expected_set.contains(open_time))
            .collect();
        let missing: Vec<i64> = expected
            .iter()
            .copied()
            .filter(|open_time| !initial_present.contains(open_time))
            .collect();
        if missing.is_empty() {
            return Ok(ReconciliationReport {
                expected: expected.len(),
                already_present: initial_present.len(),
                repaired: 0,
                unresolved: Vec::new(),
            });
        }

        let prior_status = self.hub.snapshot(&market.symbol, timeframe).await.status;
        self.hub
            .set_status(&market.symbol, LiveStatus::Degraded)
            .await;
        for range in contiguous_ranges(&missing, timeframe.duration_ms(), 1_500) {
            let range_start = *range.first().expect("range is non-empty");
            let range_last = *range.last().expect("range is non-empty");
            let range_end = range_last
                .checked_add(timeframe.duration_ms())
                .and_then(|value| value.checked_sub(1))
                .ok_or_else(|| {
                    ServiceError::InvalidData("reconciliation range overflowed".to_owned())
                })?;
            let returned = self
                .source
                .klines(
                    &market.symbol,
                    timeframe,
                    Some(range_start),
                    Some(range_end),
                    u16::try_from(range.len()).expect("range is capped at 1500"),
                )
                .await?;
            let range_set: HashSet<i64> = range.into_iter().collect();
            let native: Vec<_> = returned
                .into_iter()
                .filter(|candle| {
                    candle.symbol == market.symbol
                        && candle.timeframe == timeframe
                        && range_set.contains(&candle.open_time)
                        && candle.is_closed_at(end)
                })
                .collect();
            for candle in native {
                self.candles
                    .persist_closed_operational(&ClosedCandleWrite {
                        candle,
                        source: ClosedCandleSource::DowntimeReconciled,
                    })
                    .await?;
            }
        }

        let final_present: HashSet<i64> = self
            .candles
            .open_times(&market.symbol, timeframe, start, end)
            .await?
            .into_iter()
            .filter(|open_time| expected_set.contains(open_time))
            .collect();
        let unresolved: Vec<i64> = expected
            .iter()
            .copied()
            .filter(|open_time| !final_present.contains(open_time))
            .collect();
        let repaired = final_present.difference(&initial_present).count();
        if unresolved.is_empty()
            && matches!(prior_status, LiveStatus::Ready | LiveStatus::MarketClosed)
        {
            self.hub.set_status(&market.symbol, prior_status).await;
        }
        Ok(ReconciliationReport {
            expected: expected.len(),
            already_present: initial_present.len(),
            repaired,
            unresolved,
        })
    }

    pub async fn reconcile_to_server_time(
        &self,
        symbol: &market_domain::Symbol,
    ) -> Result<Vec<(Timeframe, ReconciliationReport)>, ServiceError> {
        let server_time = self.source.server_time().await?;
        let market = self.source.market(symbol).await?;
        let mut reports = Vec::new();
        // Higher timeframes first: lower-timeframe candles enqueue analysis work that
        // reads higher-timeframe context at the same boundary.
        for timeframe in Timeframe::ALL.into_iter().rev() {
            let Some((oldest, latest)) = self.candles.bounds(symbol, timeframe).await? else {
                continue;
            };
            if latest >= server_time {
                continue;
            }
            let start = reconcile_start(timeframe, oldest, latest);
            reports.push((
                timeframe,
                self.reconcile(&market, timeframe, start, server_time)
                    .await?,
            ));
        }
        Ok(reports)
    }
}

fn contiguous_ranges(missing: &[i64], duration: i64, maximum: usize) -> Vec<Vec<i64>> {
    let mut ranges: Vec<Vec<i64>> = Vec::new();
    for open_time in missing {
        let append = ranges.last().is_some_and(|range| {
            range.len() < maximum
                && range.last().and_then(|last| last.checked_add(duration)) == Some(*open_time)
        });
        if append {
            ranges
                .last_mut()
                .expect("last range exists")
                .push(*open_time);
        } else {
            ranges.push(vec![*open_time]);
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::reconcile_start;
    use market_domain::Timeframe;

    #[test]
    fn reconcile_start_rechecks_recent_history_but_not_before_oldest() {
        let minute = Timeframe::M1.duration_ms();
        assert_eq!(
            reconcile_start(Timeframe::M1, 0, 1_000 * minute),
            800 * minute
        );
        assert_eq!(
            reconcile_start(Timeframe::M1, 50 * minute, 100 * minute),
            50 * minute
        );
    }
}
