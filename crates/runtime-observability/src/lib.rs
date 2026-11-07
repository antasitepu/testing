#![forbid(unsafe_code)]

use serde::Serialize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

static MARKET_RECONNECT_COUNT: AtomicU64 = AtomicU64::new(0);
static MARKET_RESYNC_COUNT: AtomicU64 = AtomicU64::new(0);
static EXECUTION_EXACT_REPLAY_RECOVERY_COUNT: AtomicU64 = AtomicU64::new(0);
static EXECUTION_OHLC_RECONCILED_RECOVERY_COUNT: AtomicU64 = AtomicU64::new(0);
static SUBSCRIBER_LAG_COUNT: AtomicU64 = AtomicU64::new(0);
static DB_ACQUIRE_TIMEOUT_COUNT: AtomicU64 = AtomicU64::new(0);
static LATEST_MARKET_ACTIVITY: AtomicI64 = AtomicI64::new(0);
static LATEST_ANALYSIS_ACTIVITY: AtomicI64 = AtomicI64::new(0);
static LATEST_EXECUTION_ACTIVITY: AtomicI64 = AtomicI64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Market,
    Analysis,
    Execution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverySource {
    ExactReplay,
    OhlcReconciled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterSnapshot {
    pub market_reconnect_count: u64,
    pub market_resync_count: u64,
    pub execution_exact_replay_recovery_count: u64,
    pub execution_ohlc_reconciled_recovery_count: u64,
    pub subscriber_lag_count: u64,
    pub db_acquire_timeout_count: u64,
    pub latest_market_activity: Option<i64>,
    pub latest_analysis_activity: Option<i64>,
    pub latest_execution_activity: Option<i64>,
}

pub fn record_market_reconnect() {
    MARKET_RECONNECT_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn record_market_resync() {
    MARKET_RESYNC_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn record_execution_recovery(source: RecoverySource) {
    match source {
        RecoverySource::ExactReplay => {
            EXECUTION_EXACT_REPLAY_RECOVERY_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        RecoverySource::OhlcReconciled => {
            EXECUTION_OHLC_RECONCILED_RECOVERY_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn record_subscriber_lag() {
    SUBSCRIBER_LAG_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn record_db_acquire_timeout() {
    DB_ACQUIRE_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn record_activity(kind: ActivityKind, epoch_ms: i64) {
    let target = match kind {
        ActivityKind::Market => &LATEST_MARKET_ACTIVITY,
        ActivityKind::Analysis => &LATEST_ANALYSIS_ACTIVITY,
        ActivityKind::Execution => &LATEST_EXECUTION_ACTIVITY,
    };
    target.fetch_max(epoch_ms, Ordering::Relaxed);
}

pub fn snapshot() -> CounterSnapshot {
    CounterSnapshot {
        market_reconnect_count: MARKET_RECONNECT_COUNT.load(Ordering::Relaxed),
        market_resync_count: MARKET_RESYNC_COUNT.load(Ordering::Relaxed),
        execution_exact_replay_recovery_count: EXECUTION_EXACT_REPLAY_RECOVERY_COUNT
            .load(Ordering::Relaxed),
        execution_ohlc_reconciled_recovery_count: EXECUTION_OHLC_RECONCILED_RECOVERY_COUNT
            .load(Ordering::Relaxed),
        subscriber_lag_count: SUBSCRIBER_LAG_COUNT.load(Ordering::Relaxed),
        db_acquire_timeout_count: DB_ACQUIRE_TIMEOUT_COUNT.load(Ordering::Relaxed),
        latest_market_activity: optional_timestamp(&LATEST_MARKET_ACTIVITY),
        latest_analysis_activity: optional_timestamp(&LATEST_ANALYSIS_ACTIVITY),
        latest_execution_activity: optional_timestamp(&LATEST_EXECUTION_ACTIVITY),
    }
}

fn optional_timestamp(value: &AtomicI64) -> Option<i64> {
    match value.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value),
    }
}
