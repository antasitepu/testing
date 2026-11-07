use runtime_observability::{
    ActivityKind, RecoverySource, record_activity, record_db_acquire_timeout,
    record_execution_recovery, record_market_reconnect, record_market_resync,
    record_subscriber_lag, snapshot,
};

#[test]
fn market_reconnect_counter_is_monotonic() {
    let before = snapshot();
    record_market_reconnect();
    assert_eq!(
        snapshot().market_reconnect_count,
        before.market_reconnect_count + 1
    );
}

#[test]
fn market_resync_counter_is_monotonic() {
    let before = snapshot();
    record_market_resync();
    assert_eq!(
        snapshot().market_resync_count,
        before.market_resync_count + 1
    );
}

#[test]
fn recovery_counters_are_split_by_bounded_source() {
    let before = snapshot();
    record_execution_recovery(RecoverySource::ExactReplay);
    record_execution_recovery(RecoverySource::OhlcReconciled);
    let after = snapshot();
    assert_eq!(
        after.execution_exact_replay_recovery_count,
        before.execution_exact_replay_recovery_count + 1
    );
    assert_eq!(
        after.execution_ohlc_reconciled_recovery_count,
        before.execution_ohlc_reconciled_recovery_count + 1
    );
}

#[test]
fn subscriber_lag_counter_is_monotonic() {
    let before = snapshot();
    record_subscriber_lag();
    assert_eq!(
        snapshot().subscriber_lag_count,
        before.subscriber_lag_count + 1
    );
}

#[test]
fn db_acquire_timeout_counter_is_monotonic() {
    let before = snapshot();
    record_db_acquire_timeout();
    assert_eq!(
        snapshot().db_acquire_timeout_count,
        before.db_acquire_timeout_count + 1
    );
}

#[test]
fn latest_activity_timestamps_are_process_wide() {
    record_activity(ActivityKind::Market, 101);
    record_activity(ActivityKind::Analysis, 202);
    record_activity(ActivityKind::Execution, 303);
    let current = snapshot();
    assert_eq!(current.latest_market_activity, Some(101));
    assert_eq!(current.latest_analysis_activity, Some(202));
    assert_eq!(current.latest_execution_activity, Some(303));
}
