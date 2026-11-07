use crate::SnapshotRequest;
use analysis_domain::{ConfirmedFact, PlanProfileKey, PrimitiveVersion, Readiness};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct ClosedTimeframeFacts {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub price_tick: Decimal,
    pub boundary: i64,
    pub readiness: Readiness,
    pub facts: Vec<ConfirmedFact<Value>>,
    pub primitive_versions: Vec<PrimitiveVersion>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MissingDependency {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub boundary: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BarrierResult {
    Ready(Vec<SnapshotRequest>),
    Waiting,
    Reconcile(Vec<MissingDependency>),
}

#[derive(Clone, Debug)]
struct BoundaryState {
    first_seen_at: i64,
    inputs: HashMap<(Symbol, Timeframe), ClosedTimeframeFacts>,
}

pub struct EvaluationBarrier {
    required: HashMap<PlanProfileKey, BTreeSet<Timeframe>>,
    waiting: BTreeMap<i64, BoundaryState>,
    emitted: HashSet<i64>,
    grace_ms: i64,
}

impl EvaluationBarrier {
    pub fn new(
        required: HashMap<PlanProfileKey, BTreeSet<Timeframe>>,
        grace_ms: i64,
    ) -> Result<Self, BarrierError> {
        if required.is_empty() || grace_ms < 0 {
            return Err(BarrierError::InvalidConfiguration);
        }
        for (key, dependencies) in &required {
            if dependencies.is_empty()
                || !dependencies.contains(&key.target_timeframe)
                || dependencies.iter().any(|timeframe| {
                    *timeframe != key.target_timeframe
                        && timeframe.duration_ms() <= key.target_timeframe.duration_ms()
                })
            {
                return Err(BarrierError::InvalidConfiguration);
            }
        }
        Ok(Self {
            required,
            waiting: BTreeMap::new(),
            emitted: HashSet::new(),
            grace_ms,
        })
    }

    pub fn submit(&mut self, input: ClosedTimeframeFacts, now_ms: i64) -> BarrierResult {
        if input.boundary < 0 || now_ms < 0 || self.emitted.contains(&input.boundary) {
            return BarrierResult::Waiting;
        }
        let boundary = input.boundary;
        let state = self
            .waiting
            .entry(boundary)
            .or_insert_with(|| BoundaryState {
                first_seen_at: now_ms,
                inputs: HashMap::new(),
            });
        state.first_seen_at = state.first_seen_at.min(now_ms);
        state
            .inputs
            .entry((input.symbol.clone(), input.timeframe))
            .or_insert(input);
        self.evaluate(boundary, now_ms)
    }

    pub fn poll(&mut self, boundary: i64, now_ms: i64) -> BarrierResult {
        self.evaluate(boundary, now_ms)
    }

    fn evaluate(&mut self, boundary: i64, now_ms: i64) -> BarrierResult {
        if self.emitted.contains(&boundary) {
            return BarrierResult::Waiting;
        }
        let Some(state) = self.waiting.get(&boundary) else {
            return BarrierResult::Waiting;
        };
        let missing: Vec<_> = self
            .required_dependencies(boundary)
            .into_iter()
            .filter(|dependency| {
                !state
                    .inputs
                    .contains_key(&(dependency.symbol.clone(), dependency.timeframe))
            })
            .collect();
        if !missing.is_empty() {
            if now_ms.saturating_sub(state.first_seen_at) < self.grace_ms {
                return BarrierResult::Waiting;
            }
            return BarrierResult::Reconcile(missing);
        }
        let state = self
            .waiting
            .remove(&boundary)
            .expect("boundary state exists");
        let mut profiles: Vec<_> = self.required.iter().collect();
        profiles.sort_by(|(left, _), (right, _)| {
            right
                .target_timeframe
                .duration_ms()
                .cmp(&left.target_timeframe.duration_ms())
                .then_with(|| left.plan.cmp(&right.plan))
                .then_with(|| left.symbol.as_str().cmp(right.symbol.as_str()))
        });
        let requests = profiles
            .into_iter()
            .map(|(profile, dependencies)| {
                assemble_request(profile, dependencies, &state.inputs, boundary)
            })
            .collect();
        self.emitted.insert(boundary);
        BarrierResult::Ready(requests)
    }

    fn required_dependencies(&self, boundary: i64) -> Vec<MissingDependency> {
        let mut dependencies: Vec<_> = self
            .required
            .iter()
            .flat_map(|(profile, timeframes)| {
                timeframes.iter().map(|timeframe| MissingDependency {
                    symbol: profile.symbol.clone(),
                    timeframe: *timeframe,
                    boundary,
                })
            })
            .collect();
        dependencies.sort_by(|left, right| {
            left.symbol
                .as_str()
                .cmp(right.symbol.as_str())
                .then_with(|| {
                    left.timeframe
                        .duration_ms()
                        .cmp(&right.timeframe.duration_ms())
                })
        });
        dependencies.dedup();
        dependencies
    }
}

fn assemble_request(
    profile: &PlanProfileKey,
    dependencies: &BTreeSet<Timeframe>,
    inputs: &HashMap<(Symbol, Timeframe), ClosedTimeframeFacts>,
    boundary: i64,
) -> SnapshotRequest {
    let target = inputs
        .get(&(profile.symbol.clone(), profile.target_timeframe))
        .expect("barrier verified target dependency");
    let mut required_timeframes: Vec<_> = dependencies
        .iter()
        .copied()
        .filter(|timeframe| *timeframe != profile.target_timeframe)
        .collect();
    required_timeframes.sort_by_key(|timeframe| timeframe.duration_ms());
    let higher_timeframe_facts = required_timeframes
        .iter()
        .map(|timeframe| {
            let input = inputs
                .get(&(profile.symbol.clone(), *timeframe))
                .expect("barrier verified higher-timeframe dependency");
            (*timeframe, input.facts.clone())
        })
        .collect();
    let mut primitive_versions: Vec<_> = dependencies
        .iter()
        .flat_map(|timeframe| {
            inputs
                .get(&(profile.symbol.clone(), *timeframe))
                .expect("barrier verified primitive dependency")
                .primitive_versions
                .clone()
        })
        .collect();
    primitive_versions.sort();
    let readiness = dependencies
        .iter()
        .filter_map(|timeframe| {
            let readiness = inputs
                .get(&(profile.symbol.clone(), *timeframe))
                .expect("barrier verified readiness dependency")
                .readiness;
            match readiness {
                Readiness::NotReady { .. } => Some(readiness),
                Readiness::Ready => None,
            }
        })
        .next()
        .unwrap_or(Readiness::Ready);
    SnapshotRequest {
        profile: profile.clone(),
        symbol: profile.symbol.clone(),
        target_timeframe: profile.target_timeframe,
        price_tick: target.price_tick,
        evaluated_closed_candle_time: boundary,
        readiness,
        target_facts: target.facts.clone(),
        higher_timeframe_facts,
        required_timeframes,
        primitive_versions,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BarrierError {
    #[error("barrier requires non-empty valid dependencies and non-negative grace")]
    InvalidConfiguration,
}
