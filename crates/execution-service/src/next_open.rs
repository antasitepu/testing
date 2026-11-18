use analysis_domain::{PlanKind, SignalDefinition};
use analysis_plans::{
    PreEntryRejection, validate_mean_reversion_entry, validate_session_breakout_entry,
};
use market_domain::{AggregateTrade, Candle, MarketSession, SessionStatus};
use rust_decimal::Decimal;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingNextOpen {
    pub signal: SignalDefinition,
    pub expected_open_time: i64,
    pub buffered_trades: Vec<AggregateTrade>,
}

impl PendingNextOpen {
    pub fn decide(&self, candle: &Candle, session: Option<&MarketSession>) -> NextOpenDecision {
        if session.is_some_and(|session| {
            session.status == SessionStatus::NoTrading && session.contains(self.expected_open_time)
        }) {
            return NextOpenDecision::Wait;
        }
        if candle.symbol != self.signal.symbol || candle.timeframe != self.signal.target_timeframe {
            return NextOpenDecision::Reconcile {
                reason: "canonical open does not match pending signal market".to_owned(),
            };
        }
        if candle.open_time < self.expected_open_time {
            return NextOpenDecision::Wait;
        }
        if candle.open_time > self.expected_open_time {
            return NextOpenDecision::Reconcile {
                reason: "canonical open skipped expected open time".to_owned(),
            };
        }
        let validation = match self.signal.plan {
            PlanKind::MeanReversion => {
                Some(validate_mean_reversion_entry(&self.signal, candle.open))
            }
            PlanKind::SessionBreakout => {
                Some(validate_session_breakout_entry(&self.signal, candle.open))
            }
            _ => None,
        };
        if let Some(Err(error)) = validation {
            return NextOpenDecision::Cancel {
                reason: PreEntryCancellation::from(error),
            };
        }
        NextOpenDecision::Enter {
            time: candle.open_time,
            price: candle.open,
        }
    }

    pub fn replay_after_open(&self) -> Vec<AggregateTrade> {
        self.buffered_trades
            .iter()
            .filter(|event| event.trade_time > self.expected_open_time)
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct NextOpenGuard {
    confirmed: HashSet<(Uuid, i64)>,
}

impl NextOpenGuard {
    pub fn confirm(
        &mut self,
        pending: &PendingNextOpen,
        candle: &Candle,
        session: Option<&MarketSession>,
    ) -> NextOpenDecision {
        let decision = pending.decide(candle, session);
        if !matches!(decision, NextOpenDecision::Enter { .. }) {
            return decision;
        }
        if !self
            .confirmed
            .insert((pending.signal.signal_id, candle.open_time))
        {
            return NextOpenDecision::Duplicate;
        }
        decision
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NextOpenDecision {
    Enter { time: i64, price: Decimal },
    Cancel { reason: PreEntryCancellation },
    Reconcile { reason: String },
    Duplicate,
    Wait,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreEntryCancellation {
    TargetCrossed,
    StopCrossed,
    InsufficientRiskReward,
    MissingValidation,
    WrongPlan,
}

impl From<PreEntryRejection> for PreEntryCancellation {
    fn from(value: PreEntryRejection) -> Self {
        match value {
            PreEntryRejection::TargetCrossed => Self::TargetCrossed,
            PreEntryRejection::StopCrossed => Self::StopCrossed,
            PreEntryRejection::InsufficientRiskReward => Self::InsufficientRiskReward,
            PreEntryRejection::MissingValidation => Self::MissingValidation,
            PreEntryRejection::WrongPlan => Self::WrongPlan,
        }
    }
}
