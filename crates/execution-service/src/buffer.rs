use execution_domain::TrackingHealth;
use market_domain::AggregateTrade;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct EffectiveEventBuffer {
    max_events: usize,
    max_age_ms: i64,
    events: VecDeque<AggregateTrade>,
}

impl EffectiveEventBuffer {
    pub fn new(max_events: usize, max_age_ms: i64) -> Self {
        Self {
            max_events,
            max_age_ms,
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: AggregateTrade) -> Result<(), BufferError> {
        if let Some(previous) = self.events.back() {
            if previous.symbol == event.symbol
                && event.aggregate_trade_id <= previous.aggregate_trade_id
            {
                return Ok(());
            }
            if previous.symbol == event.symbol
                && event.aggregate_trade_id != previous.aggregate_trade_id + 1
            {
                return Err(BufferError::SequenceGap {
                    previous_id: previous.aggregate_trade_id,
                    next_id: event.aggregate_trade_id,
                });
            }
        }
        if self.events.len() >= self.max_events {
            return Err(BufferError::Overflow {
                max_events: self.max_events,
            });
        }
        self.events.push_back(event);
        Ok(())
    }

    pub fn replay_from(&self, effective_at: i64) -> Vec<AggregateTrade> {
        self.events
            .iter()
            .filter(|event| event.trade_time >= effective_at)
            .cloned()
            .collect()
    }

    pub fn prune(&mut self, now_ms: i64) {
        while self
            .events
            .front()
            .is_some_and(|event| now_ms - event.trade_time > self.max_age_ms)
        {
            self.events.pop_front();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BufferError {
    #[error("effective event buffer overflowed at {max_events} events")]
    Overflow { max_events: usize },
    #[error("aggregate trade sequence gap from {previous_id} to {next_id}")]
    SequenceGap { previous_id: i64, next_id: i64 },
}

impl BufferError {
    pub const fn tracking_health(&self) -> TrackingHealth {
        TrackingHealth::Stale
    }
}
