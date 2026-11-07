use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, FactId, PlanProfileKey, PrimitiveVersion, Readiness,
    TimeframeFacts,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRequest {
    pub profile: PlanProfileKey,
    pub symbol: Symbol,
    pub target_timeframe: Timeframe,
    pub price_tick: Decimal,
    pub evaluated_closed_candle_time: i64,
    pub readiness: Readiness,
    pub target_facts: Vec<ConfirmedFact<Value>>,
    pub higher_timeframe_facts: Vec<(Timeframe, Vec<ConfirmedFact<Value>>)>,
    pub required_timeframes: Vec<Timeframe>,
    pub primitive_versions: Vec<PrimitiveVersion>,
}

pub struct SnapshotBuilder;

impl SnapshotBuilder {
    pub fn build(mut request: SnapshotRequest) -> Result<CanonicalMarketSnapshot, SnapshotError> {
        if request.evaluated_closed_candle_time < 0 {
            return Err(SnapshotError::NegativeEvaluationTime);
        }
        if request.price_tick <= Decimal::ZERO {
            return Err(SnapshotError::NonPositivePriceTick);
        }
        validate_facts(
            &request.symbol,
            request.target_timeframe,
            request.evaluated_closed_candle_time,
            &request.target_facts,
        )?;
        sort_facts(&mut request.target_facts);

        let required: HashSet<_> = request.required_timeframes.iter().copied().collect();
        if required.len() != request.required_timeframes.len()
            || required
                .iter()
                .any(|timeframe| timeframe.duration_ms() <= request.target_timeframe.duration_ms())
        {
            return Err(SnapshotError::InvalidRequiredTimeframe);
        }
        let available: HashSet<_> = request
            .higher_timeframe_facts
            .iter()
            .map(|(timeframe, _)| *timeframe)
            .collect();
        if !required.is_subset(&available) {
            return Err(SnapshotError::MissingHigherTimeframe);
        }

        let versioned_timeframes: HashSet<_> = request
            .primitive_versions
            .iter()
            .map(|version| version.timeframe)
            .collect();
        if !versioned_timeframes.contains(&request.target_timeframe)
            || !required.is_subset(&versioned_timeframes)
        {
            return Err(SnapshotError::MissingPrimitiveVersion);
        }

        let mut higher_timeframes = Vec::with_capacity(request.higher_timeframe_facts.len());
        for (timeframe, mut facts) in request.higher_timeframe_facts {
            validate_facts(
                &request.symbol,
                timeframe,
                request.evaluated_closed_candle_time,
                &facts,
            )?;
            sort_facts(&mut facts);
            higher_timeframes.push(TimeframeFacts {
                timeframe,
                evaluated_closed_candle_time: request.evaluated_closed_candle_time,
                facts,
            });
        }
        higher_timeframes.sort_by_key(|facts| facts.timeframe.duration_ms());
        request.primitive_versions.sort();

        Ok(CanonicalMarketSnapshot {
            symbol: request.symbol,
            target_timeframe: request.target_timeframe,
            price_tick: request.price_tick,
            evaluated_closed_candle_time: request.evaluated_closed_candle_time,
            readiness: request.readiness,
            target: TimeframeFacts {
                timeframe: request.target_timeframe,
                evaluated_closed_candle_time: request.evaluated_closed_candle_time,
                facts: request.target_facts,
            },
            higher_timeframes,
            primitive_versions: request.primitive_versions,
        })
    }
}

fn validate_facts(
    symbol: &Symbol,
    timeframe: Timeframe,
    evaluated_closed_candle_time: i64,
    facts: &[ConfirmedFact<Value>],
) -> Result<(), SnapshotError> {
    if facts.iter().any(|fact| {
        &fact.id.symbol != symbol
            || fact.id.timeframe != timeframe
            || fact.id.confirmed_at > evaluated_closed_candle_time
    }) {
        return Err(SnapshotError::InvalidFactBoundary);
    }
    Ok(())
}

fn sort_facts(facts: &mut [ConfirmedFact<Value>]) {
    facts.sort_by(|left, right| fact_sort_key(&left.id).cmp(&fact_sort_key(&right.id)));
}

fn fact_sort_key(id: &FactId) -> (&str, i64, analysis_domain::FactKind, u32, &str, &str) {
    (
        id.symbol.as_str(),
        id.confirmed_at,
        id.kind,
        id.ordinal,
        &id.algorithm_version,
        &id.parameter_version,
    )
}

#[derive(Default)]
pub struct FactLedger {
    confirmed: HashMap<FactId, ConfirmedFact<Value>>,
}

impl FactLedger {
    pub fn record(&mut self, fact: ConfirmedFact<Value>) -> Result<(), FactLedgerError> {
        if let Some(existing) = self.confirmed.get(&fact.id)
            && existing.value != fact.value
        {
            return Err(FactLedgerError::ImmutableValueChanged);
        }
        self.confirmed.insert(fact.id.clone(), fact);
        Ok(())
    }

    pub fn confirmed(&self) -> Vec<ConfirmedFact<Value>> {
        let mut facts: Vec<_> = self.confirmed.values().cloned().collect();
        sort_facts(&mut facts);
        facts
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FactLedgerError {
    #[error("confirmed fact value and identity are immutable")]
    ImmutableValueChanged,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot evaluation time must not be negative")]
    NegativeEvaluationTime,
    #[error("snapshot price tick must be positive")]
    NonPositivePriceTick,
    #[error("fact identity is outside the snapshot boundary")]
    InvalidFactBoundary,
    #[error("required timeframe must be unique and higher than target")]
    InvalidRequiredTimeframe,
    #[error("required higher timeframe facts are missing")]
    MissingHigherTimeframe,
    #[error("target or required higher timeframe primitive version is missing")]
    MissingPrimitiveVersion,
}
