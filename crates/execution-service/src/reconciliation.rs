use execution_domain::{MarketCursor, TrackingHealth};
use market_domain::{AggregateTrade, MarketSession, SessionStatus, Symbol};
use uuid::Uuid;

pub(crate) fn reconciliation_event_id(
    signal_id: Uuid,
    virtual_trade_id: Uuid,
    event_time: i64,
    event_kind: &str,
) -> Uuid {
    deterministic_id(&[
        signal_id.as_bytes(),
        virtual_trade_id.as_bytes(),
        &event_time.to_be_bytes(),
        event_kind.as_bytes(),
    ])
}

pub(crate) fn reconciliation_trade_id(signal_id: Uuid, entry_time: i64, entry_kind: &str) -> Uuid {
    deterministic_id(&[
        signal_id.as_bytes(),
        &entry_time.to_be_bytes(),
        entry_kind.as_bytes(),
    ])
}

fn deterministic_id(parts: &[&[u8]]) -> Uuid {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut high = OFFSET;
    let mut low = OFFSET ^ 0x9e3779b97f4a7c15;
    for part in parts {
        for byte in *part {
            high ^= u64::from(*byte);
            high = high.wrapping_mul(PRIME);
            low ^= u64::from(*byte).rotate_left(1);
            low = low.wrapping_mul(PRIME ^ 0x517cc1b727220a95);
        }
        high ^= 0xff;
        low ^= 0x7f;
    }
    Uuid::from_u128((u128::from(high) << 64) | u128::from(low))
}

#[derive(Clone, Debug)]
pub struct RecoveryInput {
    pub symbol: Symbol,
    pub cursor: Option<MarketCursor>,
    pub exact_trades: Vec<AggregateTrade>,
    pub exact_unavailable_for_24h: bool,
    pub ohlc_terminal_possible: bool,
    pub has_pending_directive: bool,
    pub has_active_recommendation: bool,
    pub has_open_trade: bool,
    pub session: Option<MarketSession>,
}

impl RecoveryInput {
    pub fn default_for(symbol: Symbol) -> Self {
        Self {
            symbol,
            cursor: None,
            exact_trades: Vec::new(),
            exact_unavailable_for_24h: false,
            ohlc_terminal_possible: false,
            has_pending_directive: false,
            has_active_recommendation: false,
            has_open_trade: false,
            session: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
    ExactUnavailable,
    NoExactTrades,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    Exact {
        cursor: MarketCursor,
    },
    OhlcReconciled {
        cursor: Option<MarketCursor>,
        ambiguous: bool,
        reason: RecoveryReason,
    },
    MarketClosed,
}

impl RecoveryOutcome {
    pub const fn tracking_health(&self) -> TrackingHealth {
        match self {
            Self::Exact { .. } => TrackingHealth::Live,
            Self::OhlcReconciled { .. } => TrackingHealth::Reconciling,
            Self::MarketClosed => TrackingHealth::MarketClosed,
        }
    }
}

pub fn choose_recovery(input: RecoveryInput) -> RecoveryOutcome {
    if input
        .session
        .as_ref()
        .is_some_and(|session| session.status == SessionStatus::NoTrading)
    {
        return RecoveryOutcome::MarketClosed;
    }
    let last_processed = input
        .cursor
        .as_ref()
        .map(|cursor| cursor.last_processed_agg_trade_id)
        .unwrap_or(-1);
    let mut exact = input
        .exact_trades
        .into_iter()
        .filter(|trade| trade.aggregate_trade_id > last_processed)
        .collect::<Vec<_>>();
    exact.sort_by_key(|trade| trade.aggregate_trade_id);
    if let Some(last) = exact.last() {
        return RecoveryOutcome::Exact {
            cursor: MarketCursor {
                symbol: input.symbol,
                last_processed_agg_trade_id: last.aggregate_trade_id,
                last_trade_time: last.trade_time,
            },
        };
    }
    RecoveryOutcome::OhlcReconciled {
        cursor: input.cursor,
        ambiguous: input.ohlc_terminal_possible,
        reason: if input.exact_unavailable_for_24h {
            RecoveryReason::ExactUnavailable
        } else {
            RecoveryReason::NoExactTrades
        },
    }
}

#[cfg(test)]
mod tests {
    use super::reconciliation_event_id;
    use uuid::Uuid;

    #[test]
    fn deterministic_event_id_collision_domain_includes_signal_trade_time_and_kind() {
        let signal = Uuid::from_u128(1);
        let trade = Uuid::from_u128(2);
        let baseline = reconciliation_event_id(signal, trade, 3, "ENTRY_TRIGGERED");

        assert_ne!(
            baseline,
            reconciliation_event_id(Uuid::from_u128(4), trade, 3, "ENTRY_TRIGGERED")
        );
        assert_ne!(
            baseline,
            reconciliation_event_id(signal, Uuid::from_u128(4), 3, "ENTRY_TRIGGERED")
        );
        assert_ne!(
            baseline,
            reconciliation_event_id(signal, trade, 4, "ENTRY_TRIGGERED")
        );
        assert_ne!(
            baseline,
            reconciliation_event_id(signal, trade, 3, "TARGET_TOUCHED")
        );
    }
}
