use analysis_domain::PriceZone;
use execution_domain::TrackingHealth;
use execution_service::{
    LimitZoneDecision, LimitZoneError, LimitZoneGuard, proves_effective_zone_touch,
    proves_zone_touch,
};
use market_domain::{AggregateTrade, Symbol};
use rust_decimal::Decimal;

#[test]
fn direct_touch_inside_and_on_both_boundaries_proves_limit_zone_entry() {
    let zone = zone();

    assert!(proves_zone_touch(&zone, &trade(10, 4105, 1_000)));
    assert!(proves_zone_touch(&zone, &trade(11, 4100, 1_100)));
    assert!(proves_zone_touch(&zone, &trade(12, 4110, 1_200)));
}

#[test]
fn crossing_without_in_zone_event_does_not_interpolate_entry() {
    let zone = zone();

    assert!(!proves_zone_touch(&zone, &trade(10, 4099, 1_000)));
    assert!(!proves_zone_touch(&zone, &trade(11, 4111, 1_100)));
}

#[test]
fn pre_effective_and_invalidated_signals_do_not_trigger_entry() {
    let zone = zone();

    assert!(!proves_effective_zone_touch(
        &zone,
        &trade(10, 4105, 999),
        1_000,
        false
    ));
    assert!(!proves_effective_zone_touch(
        &zone,
        &trade(11, 4105, 1_000),
        1_000,
        true
    ));
    assert!(proves_effective_zone_touch(
        &zone,
        &trade(12, 4105, 1_000),
        1_000,
        false
    ));
}

#[test]
fn duplicate_event_is_ignored_without_retriggering() {
    let zone = zone();
    let mut guard = LimitZoneGuard::default();

    assert!(matches!(
        guard.observe(&zone, &trade(10, 4105, 1_000), 1_000, false),
        Ok(LimitZoneDecision::Enter {
            aggregate_trade_id: 10,
            ..
        })
    ));
    assert_eq!(
        guard.observe(&zone, &trade(10, 4106, 1_001), 1_000, false),
        Ok(LimitZoneDecision::Duplicate)
    );
}

#[test]
fn sequence_gap_marks_tracking_stale_before_entry() {
    let zone = zone();
    let mut guard = LimitZoneGuard::default();

    assert_eq!(
        guard.observe(&zone, &trade(10, 4099, 1_000), 1_000, false),
        Ok(LimitZoneDecision::Wait)
    );
    let error = guard
        .observe(&zone, &trade(12, 4105, 1_100), 1_000, false)
        .unwrap_err();

    assert_eq!(
        error,
        LimitZoneError::SequenceGap {
            previous_id: 10,
            next_id: 12
        }
    );
    assert_eq!(error.tracking_health(), TrackingHealth::Stale);
}

fn zone() -> PriceZone {
    PriceZone::new(Decimal::new(4100, 0), Decimal::new(4110, 0)).unwrap()
}

fn trade(id: i64, price: i64, trade_time: i64) -> AggregateTrade {
    AggregateTrade::new(
        Symbol::new("XAUUSDT").unwrap(),
        id,
        id,
        id,
        trade_time,
        trade_time,
        Decimal::new(price, 0),
    )
    .unwrap()
}
