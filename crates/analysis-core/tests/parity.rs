use analysis_core::{
    BarrierResult, ClosedTimeframeFacts, EvaluationBarrier, SnapshotBuilder, SnapshotCache,
    SnapshotCacheKey,
};
use analysis_domain::{PlanKind, PlanProfileKey, PrimitiveVersion, Readiness};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::{BTreeSet, HashMap};

const BOUNDARY: i64 = 1_782_220_000_000;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn barrier() -> EvaluationBarrier {
    let key = PlanProfileKey {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
    };
    EvaluationBarrier::new(
        HashMap::from([(key, BTreeSet::from([Timeframe::M1, Timeframe::M5]))]),
        1_000,
    )
    .unwrap()
}

fn input(timeframe: Timeframe) -> ClosedTimeframeFacts {
    ClosedTimeframeFacts {
        symbol: symbol(),
        timeframe,
        price_tick: Decimal::new(1, 1),
        boundary: BOUNDARY,
        readiness: Readiness::Ready,
        facts: Vec::new(),
        primitive_versions: vec![PrimitiveVersion {
            timeframe,
            primitive: "fixture".to_owned(),
            algorithm_version: "fixture-v1".to_owned(),
            parameter_version: "fixture-v1".to_owned(),
        }],
    }
}

fn requests(order: &[Timeframe], duplicate: bool) -> Vec<u8> {
    let mut barrier = barrier();
    let mut result = Vec::new();
    for timeframe in order {
        if let BarrierResult::Ready(requests) = barrier.submit(input(*timeframe), 0) {
            let request = requests.into_iter().next().unwrap();
            let key = SnapshotCacheKey::from_request(&request);
            let snapshot = SnapshotBuilder::build(request.clone()).unwrap();
            let mut cache = SnapshotCache::default();
            cache.insert(key.clone(), snapshot.clone());
            cache.clear();
            assert!(cache.get(&key).is_none());
            result = serde_json::to_vec(&SnapshotBuilder::build(request).unwrap()).unwrap();
        }
    }
    if duplicate {
        assert_eq!(
            barrier.submit(input(Timeframe::M5), 1),
            BarrierResult::Waiting
        );
    }
    result
}

#[test]
fn replay_incremental_cache_recompute_and_duplicate_delivery_match() {
    let replay = requests(&[Timeframe::M1, Timeframe::M5], false);
    let incremental = requests(&[Timeframe::M5, Timeframe::M1], false);
    let duplicate = requests(&[Timeframe::M1, Timeframe::M5], true);
    assert_eq!(replay, incremental);
    assert_eq!(replay, duplicate);
}
