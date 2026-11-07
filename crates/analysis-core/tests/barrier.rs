use analysis_core::{BarrierResult, ClosedTimeframeFacts, EvaluationBarrier, MissingDependency};
use analysis_domain::{PlanKind, PlanProfileKey, PrimitiveVersion, Readiness};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::{BTreeSet, HashMap};

const BOUNDARY: i64 = 1_782_220_000_000;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn profiles() -> HashMap<PlanProfileKey, BTreeSet<Timeframe>> {
    let mut profiles = HashMap::new();
    for timeframe in Timeframe::ALL {
        for plan in PlanKind::ALL {
            let mut required = BTreeSet::new();
            required.insert(timeframe);
            if timeframe != Timeframe::D1 {
                required.insert(Timeframe::D1);
            }
            profiles.insert(
                PlanProfileKey {
                    symbol: symbol(),
                    plan,
                    target_timeframe: timeframe,
                },
                required,
            );
        }
    }
    profiles
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
            parameter_version: format!("fixture-{}-v1", timeframe.as_str()),
        }],
    }
}

fn run_order(order: &[Timeframe]) -> Vec<(Timeframe, PlanKind)> {
    let mut barrier = EvaluationBarrier::new(profiles(), 1_000).unwrap();
    let mut ready = Vec::new();
    for (index, timeframe) in order.iter().enumerate() {
        match barrier.submit(input(*timeframe), i64::try_from(index).unwrap() * 10) {
            BarrierResult::Ready(requests) => {
                ready = requests
                    .into_iter()
                    .map(|request| (request.profile.target_timeframe, request.profile.plan))
                    .collect();
            }
            BarrierResult::Waiting => {}
            BarrierResult::Reconcile(missing) => panic!("unexpected missing {missing:?}"),
        }
    }
    ready
}

#[test]
fn simultaneous_close_is_arrival_order_independent_and_canonical() {
    let ascending = run_order(&Timeframe::ALL);
    let mut reversed = Timeframe::ALL;
    reversed.reverse();
    let descending = run_order(&reversed);
    assert_eq!(ascending, descending);
    assert_eq!(ascending.len(), 42);

    let expected_prefix: Vec<_> = PlanKind::ALL
        .into_iter()
        .map(|plan| (Timeframe::D1, plan))
        .collect();
    assert_eq!(&ascending[..6], expected_prefix.as_slice());
}

#[test]
fn grace_expiry_requests_reconciliation_without_ready_snapshot() {
    let mut barrier = EvaluationBarrier::new(profiles(), 100).unwrap();
    assert_eq!(
        barrier.submit(input(Timeframe::M1), 1_000),
        BarrierResult::Waiting
    );
    assert_eq!(barrier.poll(BOUNDARY, 1_099), BarrierResult::Waiting);
    let BarrierResult::Reconcile(missing) = barrier.poll(BOUNDARY, 1_100) else {
        panic!("missing dependency should reconcile");
    };
    assert!(missing.contains(&MissingDependency {
        symbol: symbol(),
        timeframe: Timeframe::D1,
        boundary: BOUNDARY,
    }));
}

#[test]
fn required_higher_timeframe_not_ready_propagates_to_snapshot() {
    let key = PlanProfileKey {
        symbol: symbol(),
        plan: PlanKind::Ict,
        target_timeframe: Timeframe::M1,
    };
    let mut barrier = EvaluationBarrier::new(
        HashMap::from([(key, BTreeSet::from([Timeframe::M1, Timeframe::M5]))]),
        100,
    )
    .unwrap();
    assert_eq!(
        barrier.submit(input(Timeframe::M1), 0),
        BarrierResult::Waiting
    );
    let mut higher = input(Timeframe::M5);
    higher.readiness = Readiness::NotReady {
        reason: analysis_domain::ReadinessReason::InsufficientHistory,
    };
    let BarrierResult::Ready(requests) = barrier.submit(higher, 1) else {
        panic!("dependencies are complete");
    };
    assert!(matches!(requests[0].readiness, Readiness::NotReady { .. }));
}
