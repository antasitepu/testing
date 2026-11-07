use analysis_core::{
    ConfirmedSwing, DisplacementParameters, RsiPivot, SnapshotBuilder, SnapshotRequest,
    SwingParameters, calculate_adx, calculate_atr, calculate_bollinger, calculate_chandelier,
    calculate_ema, calculate_previous_donchian, calculate_rsi, detect_displacements_with_atr,
    detect_fair_value_gaps, detect_liquidity_pools, detect_liquidity_sweeps,
    detect_regular_divergence, detect_structure, fvg_lifecycle,
};
use analysis_domain::{
    AnalysisWorkItem, ConfirmedFact, FactId, FactKind, FactLifecycle, MaterializedProfile,
    PrimitiveVersion, Readiness, ReadinessReason,
};
use analysis_service::{
    AnalysisCandleSource, AnalysisServiceError, GapStatusSource, SnapshotAvailability,
};
use async_trait::async_trait;
use market_domain::{Symbol, Timeframe};
use market_service::expected_open_times;
use market_storage::{CandleRepository, MarketRepository, SessionRepository};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

const DEFAULT_BARRIER_GRACE_MS: i64 = 10_000;

pub struct RuntimeAnalysisCandleSource {
    candles: CandleRepository,
    markets: MarketRepository,
    sessions: SessionRepository,
    barrier_grace_ms: i64,
}

impl RuntimeAnalysisCandleSource {
    pub fn new(
        candles: CandleRepository,
        markets: MarketRepository,
        sessions: SessionRepository,
    ) -> Self {
        Self {
            candles,
            markets,
            sessions,
            barrier_grace_ms: DEFAULT_BARRIER_GRACE_MS,
        }
    }

    pub fn with_barrier_grace_ms(mut self, barrier_grace_ms: i64) -> Self {
        self.barrier_grace_ms = barrier_grace_ms;
        self
    }
}

struct TimeframeFacts {
    facts: Vec<ConfirmedFact<Value>>,
    ready: bool,
    boundary_candle_missing: bool,
}

#[async_trait]
impl AnalysisCandleSource for RuntimeAnalysisCandleSource {
    async fn snapshot(
        &self,
        work: &AnalysisWorkItem,
        profile: &MaterializedProfile,
    ) -> Result<SnapshotAvailability, AnalysisServiceError> {
        let market = self
            .markets
            .load(&work.symbol)
            .await
            .map_err(|error| AnalysisServiceError::Storage {
                operation: "load runtime analysis market",
                message: error.to_string(),
            })?
            .ok_or_else(|| {
                AnalysisServiceError::InvalidData(format!("unknown market {}", work.symbol))
            })?;
        let target = self
            .timeframe_facts(work, profile, &market, profile.key.target_timeframe)
            .await?;
        let (target_facts, target_ready) = (target.facts, target.ready);
        let mut higher_timeframe_facts = Vec::new();
        let mut higher_ready = true;
        let mut dependency_missing = false;
        for timeframe in &profile.required_timeframes {
            let higher = self
                .timeframe_facts(work, profile, &market, *timeframe)
                .await?;
            higher_ready &= higher.ready;
            dependency_missing |= higher.boundary_candle_missing;
            higher_timeframe_facts.push((*timeframe, higher.facts));
        }
        // Event-time barrier: a required higher-timeframe candle closing at or before
        // this boundary may still be in flight; wait for it within the grace period,
        // then evaluate as not ready instead of using stale context.
        if dependency_missing {
            if barrier_is_open(
                work.canonical_close_time,
                super::current_unix_millis(),
                self.barrier_grace_ms,
            ) {
                return Ok(SnapshotAvailability::Waiting);
            }
            higher_ready = false;
        }
        let readiness = if !target_ready {
            Readiness::NotReady {
                reason: ReadinessReason::InsufficientHistory,
            }
        } else if !higher_ready {
            Readiness::NotReady {
                reason: ReadinessReason::MissingHigherTimeframe,
            }
        } else {
            Readiness::Ready
        };
        let snapshot = SnapshotBuilder::build(SnapshotRequest {
            profile: profile.key.clone(),
            symbol: work.symbol.clone(),
            target_timeframe: profile.key.target_timeframe,
            price_tick: market.price_tick,
            evaluated_closed_candle_time: work.canonical_close_time,
            readiness,
            target_facts,
            higher_timeframe_facts,
            required_timeframes: profile.required_timeframes.clone(),
            primitive_versions: primitive_versions(profile),
        })
        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        Ok(SnapshotAvailability::Ready(snapshot))
    }
}

impl RuntimeAnalysisCandleSource {
    async fn timeframe_facts(
        &self,
        work: &AnalysisWorkItem,
        profile: &MaterializedProfile,
        market: &market_domain::Market,
        timeframe: Timeframe,
    ) -> Result<TimeframeFacts, AnalysisServiceError> {
        let not_ready = || TimeframeFacts {
            facts: Vec::new(),
            ready: false,
            boundary_candle_missing: false,
        };
        let raw_limit = i64::try_from(profile.minimum_history_bars.max(500))
            .unwrap_or(500)
            .saturating_mul(2);
        // Bound the query by the work boundary so old backlog work still gets the
        // history that precedes it rather than the newest candles.
        let before = work.canonical_close_time.checked_add(1).ok_or_else(|| {
            AnalysisServiceError::InvalidData("work close time overflow".to_owned())
        })?;
        let candles = self
            .candles
            .latest(&work.symbol, timeframe, Some(before), raw_limit)
            .await
            .map_err(|error| AnalysisServiceError::Storage {
                operation: "load runtime analysis candles",
                message: error.to_string(),
            })?;
        let candles: Vec<_> = candles
            .into_iter()
            .filter(|candle| candle.close_time <= work.canonical_close_time)
            .collect();
        let Some(first_open_time) = candles.first().map(|candle| candle.open_time) else {
            return Ok(not_ready());
        };
        let Some(last_open_time) = candles.last().map(|candle| candle.open_time) else {
            return Ok(not_ready());
        };
        let sessions = self
            .sessions
            .between(&market.session_market, first_open_time, before)
            .await
            .map_err(|error| AnalysisServiceError::Storage {
                operation: "load runtime analysis sessions",
                message: error.to_string(),
            })?;
        let boundary_candle_missing = latest_closed_open_time(timeframe, work.canonical_close_time)
            .is_some_and(|expected_open_time| {
                last_open_time < expected_open_time
                    && !expected_open_times(
                        timeframe,
                        expected_open_time,
                        expected_open_time.saturating_add(timeframe.duration_ms()),
                        &sessions,
                    )
                    .is_empty()
            });
        let candles = crate::runtime_analysis_candles::filter_runtime_candles(
            candles,
            &sessions,
            market.onboard_time,
        )
        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        if candles.len() < profile.minimum_history_bars.min(50) {
            return Ok(TimeframeFacts {
                boundary_candle_missing,
                ..not_ready()
            });
        }
        let ready = candles.len() >= profile.minimum_history_bars;
        let facts = build_facts(&candles)
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?;
        Ok(TimeframeFacts {
            facts,
            ready,
            boundary_candle_missing,
        })
    }
}

/// Open time of the latest candle of `timeframe` that is fully closed at `close_time`.
fn latest_closed_open_time(timeframe: Timeframe, close_time: i64) -> Option<i64> {
    let duration = timeframe.duration_ms();
    let end = close_time.checked_add(1)?;
    end.checked_sub(end.rem_euclid(duration))?
        .checked_sub(duration)
}

fn barrier_is_open(close_time: i64, now_ms: i64, grace_ms: i64) -> bool {
    now_ms <= close_time.saturating_add(grace_ms)
}

fn primitive_versions(profile: &MaterializedProfile) -> Vec<PrimitiveVersion> {
    let mut timeframes = Vec::with_capacity(profile.required_timeframes.len() + 1);
    timeframes.push(profile.key.target_timeframe);
    timeframes.extend(profile.required_timeframes.iter().copied());
    timeframes
        .into_iter()
        .flat_map(|timeframe| {
            [
                primitive_version(timeframe, "structure", "structure-runtime-v1"),
                primitive_version(timeframe, "price", "price-runtime-v1"),
                primitive_version(timeframe, "liquidity", "liquidity-runtime-v1"),
                primitive_version(timeframe, "momentum", "momentum-runtime-v1"),
                primitive_version(timeframe, "trend", "trend-runtime-v1"),
                primitive_version(timeframe, "volatility", "volatility-runtime-v1"),
                primitive_version(timeframe, "range", "range-runtime-v1"),
            ]
        })
        .collect()
}

pub(super) fn primitive_version(
    timeframe: Timeframe,
    primitive: &str,
    algorithm_version: &str,
) -> PrimitiveVersion {
    PrimitiveVersion {
        timeframe,
        primitive: primitive.to_owned(),
        algorithm_version: algorithm_version.to_owned(),
        parameter_version: format!("{primitive}-{}-runtime-v1", timeframe.as_str()),
    }
}

pub fn build_facts(
    candles: &[market_domain::Candle],
) -> Result<Vec<ConfirmedFact<Value>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut facts = Vec::new();
    let timeframe = candles[0].timeframe;
    let closes: Vec<_> = candles.iter().map(|candle| candle.close).collect();
    let price_version = primitive_version(timeframe, "price", "price-runtime-v1");
    let structure_version = primitive_version(timeframe, "structure", "structure-runtime-v1");
    let liquidity_version = primitive_version(timeframe, "liquidity", "liquidity-runtime-v1");
    let momentum_version = primitive_version(timeframe, "momentum", "momentum-runtime-v1");
    let trend_version = primitive_version(timeframe, "trend", "trend-runtime-v1");
    let volatility_version = primitive_version(timeframe, "volatility", "volatility-runtime-v1");
    let last_index = candles.len().saturating_sub(1);
    push_value_fact(
        &mut facts,
        candles,
        last_index,
        FactKind::MarketClose,
        candles[last_index].close,
        &price_version,
    )?;

    let structure = detect_structure(
        candles,
        SwingParameters {
            left_bars: 2,
            right_bars: 2,
        },
        &structure_version.algorithm_version,
        &structure_version.parameter_version,
    )?;
    for swing in &structure.swings {
        push_fact(
            &mut facts,
            swing.id.clone(),
            swing,
            FactLifecycle::Confirmed,
        )?;
    }
    for event in &structure.events {
        push_fact(
            &mut facts,
            event.id.clone(),
            event,
            FactLifecycle::Confirmed,
        )?;
    }

    let atr_output = calculate_atr(candles, 14)?;
    for (index, value) in atr_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Atr,
                *value,
                &volatility_version,
            )?;
        }
    }
    if atr_output.values.iter().any(Option::is_some) {
        let displacements = detect_displacements_with_atr(
            candles,
            &atr_output.values,
            DisplacementParameters {
                minimum_body_atr: Decimal::new(8, 1),
                minimum_range_atr: Decimal::new(1, 0),
            },
            &structure_version.algorithm_version,
            &structure_version.parameter_version,
        )?;
        for displacement in &displacements {
            push_fact(
                &mut facts,
                displacement.id.clone(),
                displacement,
                FactLifecycle::Confirmed,
            )?;
            let window: Vec<_> = candles
                .iter()
                .filter(|candle| {
                    candle.close_time >= displacement.id.confirmed_at - timeframe.duration_ms() * 2
                        && candle.close_time <= displacement.id.confirmed_at
                })
                .cloned()
                .collect();
            if window.len() >= 3 {
                // The window ends at the displacement, so only later candles can fill the gap.
                let later = &candles
                    [candles.partition_point(|c| c.close_time <= displacement.id.confirmed_at)..];
                for mut fvg in detect_fair_value_gaps(
                    &window,
                    Decimal::new(1, 2),
                    Some(displacement.id.clone()),
                    &liquidity_version.algorithm_version,
                    &liquidity_version.parameter_version,
                )? {
                    fvg.lifecycle = fvg_lifecycle(fvg.direction, fvg.low, fvg.high, later);
                    push_fact(&mut facts, fvg.id.clone(), &fvg, fvg.lifecycle)?;
                }
            }
        }
    }
    for fvg in detect_fair_value_gaps(
        candles,
        Decimal::new(1, 2),
        None,
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )? {
        push_fact(&mut facts, fvg.id.clone(), &fvg, fvg.lifecycle)?;
    }
    let pools = detect_liquidity_pools(
        &structure.swings,
        Decimal::new(50, 2),
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )?;
    for pool in &pools {
        push_fact(&mut facts, pool.id.clone(), pool, pool.lifecycle)?;
    }
    let sweeps = detect_liquidity_sweeps(
        &pools,
        candles,
        Decimal::new(10, 2),
        &liquidity_version.algorithm_version,
        &liquidity_version.parameter_version,
    )?;
    for sweep in &sweeps {
        push_fact(
            &mut facts,
            sweep.id.clone(),
            sweep,
            FactLifecycle::Confirmed,
        )?;
    }
    add_latest_dealing_range(&mut facts, &structure.swings)?;

    let rsi_output = calculate_rsi(&closes, 14)?;
    for (index, value) in rsi_output.values.iter().enumerate() {
        if let Some(value) = value {
            push_payload_fact(
                &mut facts,
                candles,
                index,
                FactKind::Rsi,
                json!({"value": value, "close": candles[index].close}),
                &momentum_version,
            )?;
        }
    }
    let pivots = rsi_pivots(
        candles,
        &structure.swings,
        &rsi_output.values,
        &momentum_version,
    );
    let divergences = detect_regular_divergence(
        &structure.swings,
        &pivots,
        2,
        20,
        &momentum_version.algorithm_version,
        &momentum_version.parameter_version,
    )?;
    for divergence in &divergences {
        push_fact(
            &mut facts,
            divergence.id.clone(),
            divergence,
            FactLifecycle::Confirmed,
        )?;
    }

    let ema = calculate_ema(&closes, 50, 5)?;
    for index in 0..candles.len() {
        if let (Some(value), Some(slope)) = (ema.values[index], ema.slopes[index]) {
            push_payload_fact(
                &mut facts,
                candles,
                index,
                FactKind::Ema,
                json!({"value": value, "slope": slope}),
                &trend_version,
            )?;
        }
    }
    let adx = calculate_adx(candles, 14)?;
    for (index, value) in adx.values.iter().enumerate() {
        if let Some(value) = value {
            push_value_fact(
                &mut facts,
                candles,
                index,
                FactKind::Adx,
                *value,
                &trend_version,
            )?;
        }
    }
    let donchian = calculate_previous_donchian(candles, 20)?;
    for (index, value) in donchian.iter().enumerate() {
        if let Some(value) = value {
            let payload = DonchianRuntimeFact {
                period: value.period,
                upper: value.upper,
                lower: value.lower,
                source_end_time: value.source_end_time,
                close: candles[index].close,
            };
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Donchian, &trend_version)?,
                &payload,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    let chandelier = calculate_chandelier(candles, &atr_output.values, 22, Decimal::new(3, 0))?;
    for (index, value) in chandelier.iter().enumerate() {
        if let Some(value) = value {
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Chandelier, &trend_version)?,
                value,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    let bollinger = calculate_bollinger(&closes, 20, Decimal::new(2, 0))?;
    for (index, value) in bollinger.iter().enumerate() {
        if let Some(value) = value {
            let payload = BollingerRuntimeFact {
                period: value.period,
                deviation_multiplier: value.deviation_multiplier,
                upper: value.upper,
                middle: value.middle,
                lower: value.lower,
                deviation: value.deviation,
                bandwidth: value.bandwidth,
                zscore: value.zscore,
                close: candles[index].close,
                low: candles[index].low,
                high: candles[index].high,
            };
            push_fact_id_payload(
                &mut facts,
                fact_id(candles, index, FactKind::Bollinger, &volatility_version)?,
                &payload,
                FactLifecycle::Confirmed,
            )?;
        }
    }
    Ok(facts)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DonchianRuntimeFact {
    pub(super) period: usize,
    pub(super) upper: Decimal,
    pub(super) lower: Decimal,
    pub(super) source_end_time: i64,
    pub(super) close: Decimal,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BollingerRuntimeFact {
    pub(super) period: usize,
    pub(super) deviation_multiplier: Decimal,
    pub(super) upper: Decimal,
    pub(super) middle: Decimal,
    pub(super) lower: Decimal,
    pub(super) deviation: Decimal,
    pub(super) bandwidth: Decimal,
    pub(super) zscore: Decimal,
    pub(super) close: Decimal,
    pub(super) low: Decimal,
    pub(super) high: Decimal,
}

pub(super) fn push_value_fact(
    facts: &mut Vec<ConfirmedFact<Value>>,
    candles: &[market_domain::Candle],
    index: usize,
    kind: FactKind,
    value: Decimal,
    version: &PrimitiveVersion,
) -> Result<(), serde_json::Error> {
    push_payload_fact(
        facts,
        candles,
        index,
        kind,
        json!({"value": value}),
        version,
    )
}

pub(super) fn push_payload_fact(
    facts: &mut Vec<ConfirmedFact<Value>>,
    candles: &[market_domain::Candle],
    index: usize,
    kind: FactKind,
    value: Value,
    version: &PrimitiveVersion,
) -> Result<(), serde_json::Error> {
    push_fact_id_payload(
        facts,
        fact_id(candles, index, kind, version).expect("candle index is valid"),
        &value,
        FactLifecycle::Confirmed,
    )
}

pub(super) fn push_fact<T: Serialize>(
    facts: &mut Vec<ConfirmedFact<Value>>,
    id: FactId,
    value: &T,
    lifecycle: FactLifecycle,
) -> Result<(), serde_json::Error> {
    push_fact_id_payload(facts, id, value, lifecycle)
}

pub(super) fn push_fact_id_payload<T: Serialize>(
    facts: &mut Vec<ConfirmedFact<Value>>,
    id: FactId,
    value: &T,
    lifecycle: FactLifecycle,
) -> Result<(), serde_json::Error> {
    facts.push(ConfirmedFact {
        id,
        value: serde_json::to_value(value)?,
        lifecycle,
    });
    Ok(())
}

pub(super) fn fact_id(
    candles: &[market_domain::Candle],
    index: usize,
    kind: FactKind,
    version: &PrimitiveVersion,
) -> Result<FactId, std::num::TryFromIntError> {
    Ok(FactId {
        symbol: candles[index].symbol.clone(),
        timeframe: candles[index].timeframe,
        kind,
        confirmed_at: candles[index].close_time,
        ordinal: u32::try_from(index)?,
        algorithm_version: version.algorithm_version.clone(),
        parameter_version: version.parameter_version.clone(),
    })
}

pub(super) fn rsi_pivots(
    candles: &[market_domain::Candle],
    swings: &[ConfirmedSwing],
    rsi_values: &[Option<Decimal>],
    version: &PrimitiveVersion,
) -> Vec<RsiPivot> {
    swings
        .iter()
        .filter_map(|swing| {
            let index = candles
                .iter()
                .position(|candle| candle.open_time == swing.pivot_open_time)?;
            let value = rsi_values.get(index).copied().flatten()?;
            Some(RsiPivot {
                id: FactId {
                    symbol: swing.id.symbol.clone(),
                    timeframe: swing.id.timeframe,
                    kind: FactKind::Rsi,
                    confirmed_at: swing.confirmed_at,
                    ordinal: swing.id.ordinal,
                    algorithm_version: version.algorithm_version.clone(),
                    parameter_version: version.parameter_version.clone(),
                },
                kind: swing.kind,
                value,
                pivot_open_time: swing.pivot_open_time,
                confirmed_at: swing.confirmed_at,
            })
        })
        .collect()
}

fn add_latest_dealing_range(
    facts: &mut Vec<ConfirmedFact<Value>>,
    swings: &[ConfirmedSwing],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(low) = swings
        .iter()
        .rev()
        .find(|swing| swing.kind == analysis_core::SwingKind::Low)
    else {
        return Ok(());
    };
    let Some(high) = swings
        .iter()
        .rev()
        .find(|swing| swing.kind == analysis_core::SwingKind::High)
    else {
        return Ok(());
    };
    if low.price >= high.price {
        return Ok(());
    }
    let range =
        analysis_core::build_dealing_range(low, high, "range-runtime-v1", "range-runtime-v1")?;
    push_fact(facts, range.id.clone(), &range, FactLifecycle::Confirmed)?;
    Ok(())
}

pub struct RuntimeGapStatusSource {
    markets: Arc<MarketRepository>,
    candles: Arc<CandleRepository>,
    sessions: Arc<SessionRepository>,
}

impl RuntimeGapStatusSource {
    pub fn new(
        markets: Arc<MarketRepository>,
        candles: Arc<CandleRepository>,
        sessions: Arc<SessionRepository>,
    ) -> Self {
        Self {
            markets,
            candles,
            sessions,
        }
    }
}

#[async_trait]
impl GapStatusSource for RuntimeGapStatusSource {
    async fn has_unresolved_gap(
        &self,
        symbol: &Symbol,
        timeframe: market_domain::Timeframe,
        close_time: i64,
    ) -> Result<bool, AnalysisServiceError> {
        let Some(end) = close_time.checked_add(1) else {
            return Err(AnalysisServiceError::InvalidData(
                "gap close time overflow".to_owned(),
            ));
        };
        let Some((oldest, _latest)) =
            self.candles
                .bounds(symbol, timeframe)
                .await
                .map_err(|error| AnalysisServiceError::Storage {
                    operation: "load runtime gap candle bounds",
                    message: error.to_string(),
                })?
        else {
            return Ok(false);
        };
        let lookback = timeframe
            .duration_ms()
            .checked_mul(200)
            .ok_or_else(|| AnalysisServiceError::InvalidData("gap lookback overflow".to_owned()))?;
        let start = oldest.max(end.saturating_sub(lookback));
        if start >= end {
            return Ok(false);
        }
        let Some(market) =
            self.markets
                .load(symbol)
                .await
                .map_err(|error| AnalysisServiceError::Storage {
                    operation: "load runtime gap market",
                    message: error.to_string(),
                })?
        else {
            return Ok(true);
        };
        let sessions = self
            .sessions
            .between(&market.session_market, start, end)
            .await
            .map_err(|error| AnalysisServiceError::Storage {
                operation: "load runtime gap sessions",
                message: error.to_string(),
            })?;
        let expected = expected_open_times(timeframe, start, end, &sessions);
        if expected.is_empty() {
            return Ok(false);
        }
        let present: HashSet<i64> = self
            .candles
            .open_times(symbol, timeframe, start, end)
            .await
            .map_err(|error| AnalysisServiceError::Storage {
                operation: "load runtime gap candle open times",
                message: error.to_string(),
            })?
            .into_iter()
            .collect();
        Ok(expected
            .into_iter()
            .any(|open_time| !present.contains(&open_time)))
    }

    async fn request_reconciliation(
        &self,
        _symbol: &Symbol,
        _timeframe: market_domain::Timeframe,
        _close_time: i64,
    ) -> Result<(), AnalysisServiceError> {
        Ok(())
    }
}

#[cfg(test)]
mod related_gap_tests {
    use super::build_facts;
    use analysis_core::FairValueGap;
    use analysis_domain::{FactKind, FactLifecycle};
    use market_domain::{Candle, Symbol, Timeframe};
    use rust_decimal::Decimal;

    fn candle(index: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
        let open_time = index * Timeframe::M1.duration_ms();
        Candle::new(
            Symbol::new("XAUUSDT").unwrap(),
            Timeframe::M1,
            open_time,
            open_time + Timeframe::M1.duration_ms() - 1,
            Decimal::new(open, 1),
            Decimal::new(high, 1),
            Decimal::new(low, 1),
            Decimal::new(close, 1),
        )
        .unwrap()
    }

    /// Quiet candles, then a displacement that leaves a bullish gap from 100.5 to 102.
    fn candles_with_related_gap() -> Vec<Candle> {
        let mut candles: Vec<_> = (0..60)
            .map(|i| candle(i, 1_000, 1_005, 995, 1_000))
            .collect();
        candles.push(candle(60, 1_000, 1_010, 999, 1_009));
        candles.push(candle(61, 1_020, 1_120, 1_020, 1_120));
        candles
    }

    fn related_gap_lifecycles(candles: &[Candle]) -> (FactLifecycle, FactLifecycle) {
        build_facts(candles)
            .unwrap()
            .into_iter()
            .filter(|fact| fact.id.kind == FactKind::FairValueGap)
            .find_map(|fact| {
                let gap: FairValueGap = serde_json::from_value(fact.value).unwrap();
                (gap.related_displacement_id.is_some() && gap.low == Decimal::new(1_005, 1))
                    .then_some((fact.lifecycle, gap.lifecycle))
            })
            .expect("related gap")
    }

    #[test]
    fn related_gap_stays_open_until_a_later_candle_fills_it() {
        let mut candles = candles_with_related_gap();
        assert_eq!(
            related_gap_lifecycles(&candles),
            (FactLifecycle::Confirmed, FactLifecycle::Confirmed)
        );

        candles.push(candle(62, 1_120, 1_120, 1_000, 1_010));

        assert_eq!(
            related_gap_lifecycles(&candles),
            (FactLifecycle::Filled, FactLifecycle::Filled)
        );
    }
}

#[cfg(test)]
mod barrier_tests {
    use super::{barrier_is_open, latest_closed_open_time};
    use market_domain::Timeframe;

    #[test]
    fn latest_closed_open_time_matches_boundary_candle() {
        // 1m work closing at 10:59:59.999 depends on the 1h candle opened at 10:00.
        let close = 11 * 3_600_000 - 1;
        assert_eq!(
            latest_closed_open_time(Timeframe::H1, close),
            Some(10 * 3_600_000)
        );
        // Mid-hour the latest closed 1h candle is the previous one.
        assert_eq!(
            latest_closed_open_time(Timeframe::H1, close + 60_000),
            Some(10 * 3_600_000)
        );
        assert_eq!(
            latest_closed_open_time(Timeframe::M1, close),
            Some(close + 1 - 60_000)
        );
    }

    #[test]
    fn barrier_waits_only_within_grace() {
        assert!(barrier_is_open(1_000, 1_000, 10_000));
        assert!(barrier_is_open(1_000, 11_000, 10_000));
        assert!(!barrier_is_open(1_000, 11_001, 10_000));
    }
}
