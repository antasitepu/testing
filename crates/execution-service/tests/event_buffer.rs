use execution_domain::TrackingHealth;
use execution_service::{BufferError, EffectiveEventBuffer};
use market_domain::{AggregateTrade, Symbol};
use rust_decimal::Decimal;

#[test]
fn replay_from_effective_time_includes_later_events_and_excludes_earlier_touch() {
    let symbol = symbol();
    let mut buffer = EffectiveEventBuffer::new(8, 10_000);
    buffer.push(trade(&symbol, 10, 900, 4105)).unwrap();
    buffer.push(trade(&symbol, 11, 1_000, 4106)).unwrap();
    buffer.push(trade(&symbol, 12, 1_100, 4107)).unwrap();

    let replayed = buffer.replay_from(1_000);

    assert_eq!(
        replayed
            .iter()
            .map(|event| event.aggregate_trade_id)
            .collect::<Vec<_>>(),
        vec![11, 12]
    );
}

#[test]
fn prune_removes_events_older_than_max_age() {
    let symbol = symbol();
    let mut buffer = EffectiveEventBuffer::new(8, 200);
    buffer.push(trade(&symbol, 10, 900, 4105)).unwrap();
    buffer.push(trade(&symbol, 11, 1_000, 4106)).unwrap();

    buffer.prune(1_050);
    assert_eq!(buffer.replay_from(0).len(), 2);

    buffer.prune(1_101);
    assert_eq!(
        buffer
            .replay_from(0)
            .iter()
            .map(|event| event.aggregate_trade_id)
            .collect::<Vec<_>>(),
        vec![11]
    );
}

#[test]
fn overflow_marks_tracking_stale_instead_of_silently_dropping_events() {
    let symbol = symbol();
    let mut buffer = EffectiveEventBuffer::new(1, 10_000);
    buffer.push(trade(&symbol, 10, 1_000, 4105)).unwrap();

    let error = buffer.push(trade(&symbol, 11, 1_100, 4106)).unwrap_err();

    assert_eq!(error, BufferError::Overflow { max_events: 1 });
    assert_eq!(error.tracking_health(), TrackingHealth::Stale);
    assert_eq!(buffer.replay_from(0).len(), 1);
}

#[test]
fn aggregate_trade_sequence_gap_marks_tracking_stale() {
    let symbol = symbol();
    let mut buffer = EffectiveEventBuffer::new(8, 10_000);
    buffer.push(trade(&symbol, 10, 1_000, 4105)).unwrap();

    let error = buffer.push(trade(&symbol, 12, 1_100, 4106)).unwrap_err();

    assert_eq!(
        error,
        BufferError::SequenceGap {
            previous_id: 10,
            next_id: 12
        }
    );
    assert_eq!(error.tracking_health(), TrackingHealth::Stale);
    assert_eq!(buffer.replay_from(0).len(), 1);
}

fn trade(symbol: &Symbol, id: i64, trade_time: i64, price: i64) -> AggregateTrade {
    AggregateTrade::new(
        symbol.clone(),
        id,
        id,
        id,
        trade_time,
        trade_time,
        Decimal::new(price, 0),
    )
    .unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}
