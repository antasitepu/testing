use analysis_core::{
    FactLedger, SnapshotBuilder, SnapshotCache, SnapshotCacheKey, SnapshotRequest,
};
use analysis_domain::{
    ConfirmedFact, FactId, FactKind, FactLifecycle, PlanKind, PlanProfileKey, PrimitiveVersion,
    Readiness, ReadinessReason,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::json;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn fact(timeframe: Timeframe, ordinal: u32, value: i64) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id: FactId {
            symbol: symbol(),
            timeframe,
            kind: FactKind::SwingHigh,
            confirmed_at: 1_000,
            ordinal,
            algorithm_version: "structure-v1".to_owned(),
            parameter_version: "structure-1m-v1".to_owned(),
        },
        value: json!({"price": value}),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn versions() -> Vec<PrimitiveVersion> {
    vec![
        PrimitiveVersion {
            timeframe: Timeframe::M1,
            primitive: "structure".to_owned(),
            algorithm_version: "structure-v1".to_owned(),
            parameter_version: "structure-1m-v1".to_owned(),
        },
        PrimitiveVersion {
            timeframe: Timeframe::M5,
            primitive: "structure".to_owned(),
            algorithm_version: "structure-v1".to_owned(),
            parameter_version: "structure-5m-v1".to_owned(),
        },
    ]
}

fn request(readiness: Readiness) -> SnapshotRequest {
    SnapshotRequest {
        profile: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Ict,
            target_timeframe: Timeframe::M1,
        },
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: 120_000,
        readiness,
        target_facts: vec![fact(Timeframe::M1, 2, 4_102), fact(Timeframe::M1, 1, 4_101)],
        higher_timeframe_facts: vec![(Timeframe::M5, vec![fact(Timeframe::M5, 1, 4_105)])],
        required_timeframes: vec![Timeframe::M5],
        primitive_versions: versions(),
    }
}

#[test]
fn not_ready_is_distinct_from_a_valid_snapshot_without_setup_state() {
    let not_ready = SnapshotBuilder::build(request(Readiness::NotReady {
        reason: ReadinessReason::InsufficientHistory,
    }))
    .unwrap();
    let ready = SnapshotBuilder::build(request(Readiness::Ready)).unwrap();
    assert_ne!(not_ready.readiness, ready.readiness);

    let json = serde_json::to_value(ready).unwrap();
    assert!(json.get("setupState").is_none());
    assert!(json.get("candidateFacts").is_none());
}

#[test]
fn confirmed_identity_is_immutable_but_lifecycle_can_advance() {
    let mut ledger = FactLedger::default();
    let original = fact(Timeframe::M1, 1, 4_101);
    ledger.record(original.clone()).unwrap();

    let mut lifecycle_update = original.clone();
    lifecycle_update.lifecycle = FactLifecycle::Filled;
    ledger.record(lifecycle_update.clone()).unwrap();
    assert_eq!(ledger.confirmed()[0], lifecycle_update);

    let mut rewritten = original;
    rewritten.value = json!({"price": 9999});
    assert!(ledger.record(rewritten).is_err());
}

#[test]
fn snapshot_requires_all_higher_timeframes_and_primitive_versions() {
    let mut missing_higher = request(Readiness::Ready);
    missing_higher.higher_timeframe_facts.clear();
    assert!(SnapshotBuilder::build(missing_higher).is_err());

    let mut missing_version = request(Readiness::Ready);
    missing_version
        .primitive_versions
        .retain(|version| version.timeframe != Timeframe::M5);
    assert!(SnapshotBuilder::build(missing_version).is_err());
}

#[test]
fn cache_deletion_and_replay_are_byte_deterministic() {
    let request = request(Readiness::Ready);
    let key = SnapshotCacheKey::from_request(&request);
    let first = SnapshotBuilder::build(request.clone()).unwrap();
    let first_bytes = serde_json::to_vec(&first).unwrap();

    let mut cache = SnapshotCache::default();
    cache.insert(key.clone(), first);
    assert!(cache.get(&key).is_some());
    cache.clear();
    assert!(cache.get(&key).is_none());

    let mut replay_request = request;
    replay_request.target_facts.reverse();
    replay_request.higher_timeframe_facts.reverse();
    replay_request.primitive_versions.reverse();
    let replayed = SnapshotBuilder::build(replay_request).unwrap();
    assert_eq!(first_bytes, serde_json::to_vec(&replayed).unwrap());
}
