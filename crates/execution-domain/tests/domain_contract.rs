use execution_domain::{TrackedLevelKind, TrackingHealth, TradeState};

#[test]
fn closed_trade_is_terminal() {
    assert!(TradeState::Closed.is_terminal());
}

#[test]
fn market_closed_is_not_stale() {
    assert_ne!(TrackingHealth::MarketClosed, TrackingHealth::Stale);
}

#[test]
fn reference_level_is_not_executable() {
    assert!(!TrackedLevelKind::Reference.is_terminal());
}
