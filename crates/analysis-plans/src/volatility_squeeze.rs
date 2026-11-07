//! Volatility Squeeze Breakout, `squeeze-v1`.
//!
//! Pre-registered in
//! `docs/superpowers/specs/2026-11-07-volatility-squeeze-breakout-plan-design.md`:
//! at least 6 closure-clean bars with Bollinger(20, 2) inside Keltner (shared
//! SMA20 midline, 1.5 x ATR14), then a release and, within 3 bars, a close that
//! clears a band by 0.1 ATR. Entry at the next open; thesis is the frozen
//! middle, hard stop 0.5 ATR beyond it, 2R target, exit at the next open when
//! TP1 has not filled after 20 bars.

use crate::common::{
    SharedTradeState, TargetExtensionRequest, apply_shared_trade_feedback, deterministic_uuid,
    empty_evaluation, evidence, htf_context, immutable_extension_target, management_directive,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    MaterializedProfile, PlanFeedback, PlanKind, ReferenceLevel, SetupState, SignalDefinition,
    SignalEvent, SignalEventKind, TradeState, VolatilitySqueezeParameters,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct VolatilitySqueezeEvaluator;

/// ATR period of the runtime fact builder; the ATR fact does not carry it.
const RUNTIME_ATR_PERIOD: i32 = 14;
/// A provisional target far enough away that only the hard stop of the first
/// execution-plan pass matters; the real target is 2R from that stop.
const PROVISIONAL_TARGET_ATR: i64 = 1_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BandValue {
    period: usize,
    deviation_multiplier: Decimal,
    upper: Decimal,
    middle: Decimal,
    lower: Decimal,
    deviation: Decimal,
    close: Decimal,
}

struct Bar {
    id: FactId,
    band: BandValue,
    atr: Option<(FactId, Decimal)>,
    /// No closure jump inside the Bollinger window or before it.
    clean: bool,
    squeeze_on: bool,
}

struct SqueezeSetup {
    direction: Direction,
    squeeze_start: FactId,
    release: FactId,
    breakout: FactId,
    atr_id: FactId,
    atr: Decimal,
    band_level: Decimal,
    middle: Decimal,
    close: Decimal,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SqueezeInternal {
    /// `confirmed_at` of the last release that produced a signal.
    last_consumed_release: Option<i64>,
    /// Fill time of the open trade, for the TP1 time exit.
    entry_time: Option<i64>,
    #[serde(default)]
    shared_trade: SharedTradeState,
}

impl PlanEvaluator for VolatilitySqueezeEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::VolatilitySqueeze
    }

    fn algorithm_version(&self) -> &'static str {
        "squeeze-v1"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::VolatilitySqueeze
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::VolatilitySqueeze(parameters) = &profile.parameters
        else {
            return Err(PlanError::SignalIdentityMismatch);
        };
        let mut next_state = state.clone();
        let mut internal: SqueezeInternal = if next_state.internal.is_null() {
            SqueezeInternal::default()
        } else {
            serde_json::from_value(next_state.internal.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?
        };
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
            apply_shared_trade_feedback(&mut internal.shared_trade, &next_state, item)?;
            if let PlanFeedback::ActualEntry {
                signal_id, time, ..
            } = item
                && next_state
                    .active_signal
                    .as_ref()
                    .is_some_and(|signal| signal.signal_id == *signal_id)
                && internal.entry_time.is_none()
            {
                internal.entry_time = Some(*time);
            }
        }
        if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
            internal.shared_trade = SharedTradeState::default();
            internal.entry_time = None;
            next_state.clear_terminal_trade();
            store_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_none() {
            internal.entry_time = None;
        }
        store_internal(&mut next_state, &internal)?;
        if snapshot.readiness != analysis_domain::Readiness::Ready {
            next_state.readiness = snapshot.readiness;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_some() {
            return manage_active(next_state, profile, snapshot, parameters, internal);
        }
        let Some(setup) = find_setup(snapshot, parameters, internal.last_consumed_release)? else {
            next_state.setup_state = SetupState::Watching;
            return Ok(empty_evaluation(next_state));
        };
        let Some(signal) = build_signal(profile, snapshot, parameters, &setup)? else {
            next_state.setup_state = SetupState::Watching;
            return Ok(empty_evaluation(next_state));
        };
        if let CandidateDecision::Rejected(rejected) =
            create_signal(&mut next_state, signal.clone())?
        {
            return Ok(PlanEvaluation {
                next_state,
                signal: None,
                events: Vec::new(),
                directives: Vec::new(),
                rejected_candidates: vec![rejected],
            });
        }
        internal.last_consumed_release = Some(setup.release.confirmed_at);
        store_internal(&mut next_state, &internal)?;
        let event = SignalEvent {
            event_id: deterministic_uuid(&[signal.signal_id.as_bytes(), b"ENTRY_AVAILABLE"]),
            signal_id: signal.signal_id,
            sequence: next_state.next_event_sequence,
            kind: SignalEventKind::EntryAvailable,
            effective_at: signal.effective_at,
            recorded_at: signal.detected_at,
            virtual_trade_id: None,
            price: None,
            reason: None,
        };
        append_event(&mut next_state, &event)?;
        Ok(PlanEvaluation {
            next_state,
            signal: Some(signal),
            events: vec![event],
            directives: Vec::new(),
            rejected_candidates: Vec::new(),
        })
    }
}

fn store_internal(
    state: &mut PlanRuntimeState,
    internal: &SqueezeInternal,
) -> Result<(), PlanError> {
    state.internal = serde_json::to_value(internal)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(())
}

/// Target-timeframe bars in time order, with closure cleanliness and squeeze state.
fn bars(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &VolatilitySqueezeParameters,
) -> Result<Vec<Bar>, PlanError> {
    let atr: HashMap<i64, (FactId, Decimal)> = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Atr)
        .map(|fact| {
            let value = fact
                .value
                .get("value")
                .cloned()
                .unwrap_or_else(|| fact.value.clone());
            let value: Decimal = serde_json::from_value(value)
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok((fact.id.confirmed_at, (fact.id.clone(), value)))
        })
        .collect::<Result<_, PlanError>>()?;
    let mut bands = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Bollinger)
        .map(|fact| {
            let band: BandValue = serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok((fact.id.clone(), band))
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    bands.sort_by_key(|(id, _)| id.confirmed_at);
    let duration = snapshot.target_timeframe.duration_ms();
    let window = usize::try_from(parameters.bollinger_period).unwrap_or(usize::MAX);
    let jump_before: Vec<bool> = (0..bands.len())
        .map(|index| {
            index == 0 || bands[index].0.confirmed_at - bands[index - 1].0.confirmed_at > duration
        })
        .collect();
    Ok(bands
        .into_iter()
        .enumerate()
        .map(|(index, (id, band))| {
            // Bars index-window ..= index must be contiguous: the Bollinger window
            // plus the bar before it.
            let clean = index >= window
                && jump_before[index + 1 - window..=index]
                    .iter()
                    .all(|jump| !jump);
            let atr = atr.get(&id.confirmed_at).cloned();
            let squeeze_on = atr.as_ref().is_some_and(|(_, atr)| {
                parameters.bollinger_deviation * band.deviation
                    < parameters.keltner_atr_multiplier * *atr
            });
            Bar {
                id,
                band,
                atr,
                clean,
                squeeze_on,
            }
        })
        .collect())
}

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &VolatilitySqueezeParameters,
    last_consumed_release: Option<i64>,
) -> Result<Option<SqueezeSetup>, PlanError> {
    if parameters.atr_period != RUNTIME_ATR_PERIOD {
        return Ok(None);
    }
    let bars = bars(snapshot, parameters)?;
    let Some(latest) = bars.last() else {
        return Ok(None);
    };
    if latest.id.confirmed_at != snapshot.evaluated_closed_candle_time
        || latest.band.period != usize::try_from(parameters.bollinger_period).unwrap_or_default()
        || latest.band.deviation_multiplier != parameters.bollinger_deviation
    {
        return Ok(None);
    }
    let last = bars.len() - 1;
    // The release follows the last squeeze bar; every bar from it to the latest
    // must be closure-clean with the squeeze off.
    let Some(last_squeeze) = bars.iter().rposition(|bar| bar.squeeze_on) else {
        return Ok(None);
    };
    let release = last_squeeze + 1;
    let window = usize::try_from(parameters.breakout_window_bars).unwrap_or_default();
    if release > last || window == 0 || last - release >= window {
        return Ok(None);
    }
    if bars[release..=last].iter().any(|bar| !bar.clean) {
        return Ok(None);
    }
    if last_consumed_release.is_some_and(|consumed| bars[release].id.confirmed_at <= consumed) {
        return Ok(None);
    }
    let run = bars[..release]
        .iter()
        .rev()
        .take_while(|bar| bar.clean && bar.squeeze_on)
        .count();
    if run < usize::try_from(parameters.minimum_squeeze_bars).unwrap_or(usize::MAX) {
        return Ok(None);
    }
    // The latest bar must be the first decisive close outside a band after the release.
    if bars[release..last]
        .iter()
        .any(|bar| breakout_direction(bar, parameters).is_some())
    {
        return Ok(None);
    }
    let Some(direction) = breakout_direction(latest, parameters) else {
        return Ok(None);
    };
    let Some((atr_id, atr)) = latest.atr.clone() else {
        return Ok(None);
    };
    Ok(Some(SqueezeSetup {
        direction,
        squeeze_start: bars[release - run].id.clone(),
        release: bars[release].id.clone(),
        breakout: latest.id.clone(),
        atr_id,
        atr,
        band_level: match direction {
            Direction::Buy => latest.band.upper,
            _ => latest.band.lower,
        },
        middle: latest.band.middle,
        close: latest.band.close,
    }))
}

fn breakout_direction(bar: &Bar, parameters: &VolatilitySqueezeParameters) -> Option<Direction> {
    let (_, atr) = bar.atr.as_ref()?;
    let margin = parameters.breakout_margin_atr * *atr;
    if bar.band.close - bar.band.upper >= margin {
        Some(Direction::Buy)
    } else if bar.band.lower - bar.band.close >= margin {
        Some(Direction::Sell)
    } else {
        None
    }
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &VolatilitySqueezeParameters,
    setup: &SqueezeSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let entry = setup.close;
    let provisional_target = match setup.direction {
        Direction::Buy => entry + setup.atr * Decimal::from(PROVISIONAL_TARGET_ATR),
        Direction::Sell => entry - setup.atr * Decimal::from(PROVISIONAL_TARGET_ATR),
        Direction::None => return Ok(None),
    };
    if provisional_target <= Decimal::ZERO {
        return Ok(None);
    }
    // The hard stop does not depend on the target, so the first pass fixes the
    // risk and the second places the 2R target from it.
    let Some(provisional) = materialize_execution_plan(
        setup.direction,
        entry,
        setup.middle,
        provisional_target,
        setup.atr,
        parameters.stop_buffer_atr,
        Decimal::ZERO,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let risk = (entry - provisional.hard_stop).abs();
    let analytical_target = match setup.direction {
        Direction::Buy => entry + risk * parameters.target_rr,
        Direction::Sell => entry - risk * parameters.target_rr,
        Direction::None => return Ok(None),
    };
    if risk <= Decimal::ZERO || analytical_target <= Decimal::ZERO {
        return Ok(None);
    }
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        entry,
        setup.middle,
        analytical_target,
        setup.atr,
        parameters.stop_buffer_atr,
        Decimal::ZERO,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let stop_loss = execution_plan.hard_stop;
    let risk_reward = ((analytical_target - entry).abs() / (entry - stop_loss).abs()).round_dp(6);
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("squeeze effective time overflow".to_owned()))?;
    let expires_at = effective_at.checked_add(profile.key.target_timeframe.duration_ms());
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"VOLATILITY_SQUEEZE",
        &evaluated.to_be_bytes(),
    ]);
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::VolatilitySqueeze,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction: setup.direction,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(entry),
        stop_loss,
        primary_target: analytical_target,
        execution_plan: Some(execution_plan),
        reference_levels: vec![
            ReferenceLevel {
                kind: "BREAKOUT_BAND".to_owned(),
                price: setup.band_level,
            },
            ReferenceLevel {
                kind: "SQUEEZE_MIDDLE".to_owned(),
                price: setup.middle,
            },
        ],
        risk_reward,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: vec![
            evidence(&setup.squeeze_start, "SQUEEZE_START"),
            evidence(&setup.release, "SQUEEZE_RELEASE"),
            evidence(&setup.breakout, "BAND_BREAKOUT"),
            evidence(&setup.atr_id, "BREAKOUT_ATR"),
        ],
        htf_context: Some(htf_context(snapshot, setup.direction, None)),
        effective_at,
        detected_at: effective_at,
        published_at: effective_at,
        release_id: profile.versions.release_id,
        algorithm_version: profile.versions.algorithm_version.clone(),
        parameter_version: profile.versions.parameter_version.clone(),
        created_at: effective_at,
        expires_at,
    }))
}

fn manage_active(
    mut state: PlanRuntimeState,
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &VolatilitySqueezeParameters,
    mut internal: SqueezeInternal,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    if state.trade_state == TradeState::NotEntered {
        if evaluated > signal.evaluated_at {
            return directive_evaluation(
                state,
                &signal,
                evaluated,
                DirectiveKind::ExpireEntry,
                "SQUEEZE_NEXT_OPEN_MISSED",
            );
        }
        return Ok(empty_evaluation(state));
    }
    if state.trade_state != TradeState::Open {
        return Ok(empty_evaluation(state));
    }
    if let Some(directive) = shared_thesis_invalidation_directive(&mut state, snapshot)? {
        store_internal(&mut state, &internal)?;
        return Ok(single_directive(state, directive));
    }
    if state.tp1_filled_at.is_none()
        && let Some(entry_time) = internal.entry_time
    {
        let closed_since_entry = snapshot
            .target
            .facts
            .iter()
            .filter(|fact| {
                fact.id.kind == FactKind::Bollinger
                    && fact.id.confirmed_at > entry_time
                    && fact.id.confirmed_at <= evaluated
            })
            .count();
        if closed_since_entry >= usize::try_from(parameters.max_bars_to_tp1).unwrap_or(usize::MAX) {
            return directive_evaluation(
                state,
                &signal,
                evaluated,
                DirectiveKind::ExitAtNextOpen,
                "SQUEEZE_NO_EXPANSION",
            );
        }
    }
    if let Some(directive) = shared_profit_protection_directive(
        &mut state,
        &mut internal.shared_trade,
        snapshot,
        &profile.profit_protection,
        &profile.position_management,
    )? {
        store_internal(&mut state, &internal)?;
        return Ok(single_directive(state, directive));
    }
    if let Some(directive) = shared_target_extension_directive(
        &mut state,
        &mut internal.shared_trade,
        snapshot,
        TargetExtensionRequest {
            plan_supplied_target: immutable_extension_target(&signal),
            position_management: &profile.position_management,
            minimum_rr: parameters.target_rr,
        },
    )? {
        store_internal(&mut state, &internal)?;
        return Ok(single_directive(state, directive));
    }
    store_internal(&mut state, &internal)?;
    Ok(empty_evaluation(state))
}

fn directive_evaluation(
    state: PlanRuntimeState,
    signal: &SignalDefinition,
    evaluated: i64,
    kind: DirectiveKind,
    reason: &str,
) -> Result<PlanEvaluation, PlanError> {
    let directive = management_directive(&state, signal, evaluated, kind, None, reason)?;
    Ok(single_directive(state, directive))
}

fn single_directive(
    state: PlanRuntimeState,
    directive: analysis_domain::ManagementDirective,
) -> PlanEvaluation {
    PlanEvaluation {
        next_state: state,
        signal: None,
        events: Vec::new(),
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    }
}
