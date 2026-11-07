use execution_domain::{MarketCursor, TrackingHealth};
use execution_service::{
    ExactReplayPlan, RecoveryInput, RecoveryOutcome, RecoveryReason, choose_recovery,
};
use market_domain::{AggregateTrade, MarketSession, SessionStatus, Symbol};
use rust_decimal::Decimal;

#[test]
fn exact_replay_requests_from_cursor_plus_one_and_ignores_duplicate_first_result() {
    let initial_cursor = cursor(100);
    let plan = ExactReplayPlan::from_cursor(initial_cursor.clone(), 1_000);
    assert_eq!(plan.from_id, 101);

    let outcome = choose_recovery(RecoveryInput {
        cursor: Some(initial_cursor),
        exact_trades: vec![trade(100), trade(101), trade(102)],
        ..RecoveryInput::default_for(symbol())
    });

    assert_eq!(
        outcome,
        RecoveryOutcome::Exact {
            cursor: cursor(102)
        }
    );
}

#[test]
fn unavailable_exact_history_falls_back_to_conservative_ohlc() {
    let outcome = choose_recovery(RecoveryInput {
        cursor: Some(cursor(100)),
        exact_unavailable_for_24h: true,
        ohlc_terminal_possible: true,
        ..RecoveryInput::default_for(symbol())
    });

    assert_eq!(
        outcome,
        RecoveryOutcome::OhlcReconciled {
            cursor: Some(cursor(100)),
            ambiguous: true,
            reason: RecoveryReason::ExactUnavailable
        }
    );
}

#[test]
fn empty_exact_replay_with_open_state_uses_ohlc_fallback_not_live_silence() {
    let outcome = choose_recovery(RecoveryInput {
        cursor: Some(cursor(100)),
        has_open_trade: true,
        ohlc_terminal_possible: false,
        ..RecoveryInput::default_for(symbol())
    });

    assert_eq!(
        outcome,
        RecoveryOutcome::OhlcReconciled {
            cursor: Some(cursor(100)),
            ambiguous: false,
            reason: RecoveryReason::NoExactTrades
        }
    );
}

#[test]
fn pending_directive_and_active_recommendation_keep_reconciliation_idempotent() {
    let first = choose_recovery(RecoveryInput {
        cursor: Some(cursor(100)),
        has_pending_directive: true,
        has_active_recommendation: true,
        exact_trades: vec![trade(101)],
        ..RecoveryInput::default_for(symbol())
    });
    let second = choose_recovery(RecoveryInput {
        cursor: Some(cursor(101)),
        has_pending_directive: true,
        has_active_recommendation: true,
        exact_trades: vec![trade(101)],
        ..RecoveryInput::default_for(symbol())
    });

    assert_eq!(
        first,
        RecoveryOutcome::Exact {
            cursor: cursor(101)
        }
    );
    assert_eq!(
        second,
        RecoveryOutcome::OhlcReconciled {
            cursor: Some(cursor(101)),
            ambiguous: false,
            reason: RecoveryReason::NoExactTrades
        }
    );
}

#[test]
fn market_closed_session_returns_market_closed_and_recovery_health_states_are_explicit() {
    let outcome = choose_recovery(RecoveryInput {
        session: Some(
            MarketSession::new("COMMODITY", SessionStatus::NoTrading, 0, 1_000, 1).unwrap(),
        ),
        ..RecoveryInput::default_for(symbol())
    });

    assert_eq!(outcome, RecoveryOutcome::MarketClosed);
    assert_eq!(
        RecoveryOutcome::MarketClosed.tracking_health(),
        TrackingHealth::MarketClosed
    );
    assert_eq!(
        RecoveryOutcome::OhlcReconciled {
            cursor: None,
            ambiguous: true,
            reason: RecoveryReason::ExactUnavailable
        }
        .tracking_health(),
        TrackingHealth::Reconciling
    );
}

fn cursor(id: i64) -> MarketCursor {
    MarketCursor {
        symbol: symbol(),
        last_processed_agg_trade_id: id,
        last_trade_time: id * 100,
    }
}

fn trade(id: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol(),
        id,
        id,
        id,
        id * 100,
        id * 100,
        Decimal::new(4_100 + id, 0),
    )
    .unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}
