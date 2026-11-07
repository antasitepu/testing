//! Session Opening Range Breakout, `orb-v1`.
//!
//! Pre-registered in
//! `docs/superpowers/specs/2026-11-07-session-opening-range-breakout-plan-design.md`:
//! a close beyond the complete Asia range ([00:00, 07:00) UTC) during
//! [07:00, 12:00) UTC enters at the next open, stop at the range midpoint, 2R
//! target, exit at the 20:00 UTC open, one trade per UTC day.

use crate::common::{deterministic_uuid, empty_evaluation, evidence, management_directive};
use crate::mean_reversion::{PreEntryRejection, check_pre_entry};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::SessionRangeFact;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction,
    FactKind, MaterializedProfile, PlanFeedback, PlanKind, PreEntryValidation, ReferenceLevel,
    SessionBreakoutParameters, SetupState, SignalDefinition, SignalEvent, SignalEventKind,
    TradeState,
};
use market_domain::Timeframe;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct SessionBreakoutEvaluator;

const DAY_MS: i64 = 86_400_000;
const MINUTE_MS: i64 = 60_000;
/// ATR period of the runtime fact builder, which computes `atrAtRange`.
const RUNTIME_ATR_PERIOD: i32 = 14;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionInternal {
    /// UTC day index whose breakout has been used, filled or not.
    last_session_day: Option<i64>,
}

/// Only 5m, 15m, 30m and 1h divide the range into whole bars and are agreed.
pub const fn session_breakout_runs_on(timeframe: Timeframe) -> bool {
    matches!(
        timeframe,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
    )
}

/// Next-open checks for SESSION_BREAKOUT: cancel when the open is at or through
/// the stop or the target, or when the reward/risk at the open is below the
/// signal's minimum.
pub fn validate_session_breakout_entry(
    signal: &SignalDefinition,
    actual_open: Decimal,
) -> Result<Decimal, PreEntryRejection> {
    if signal.plan != PlanKind::SessionBreakout {
        return Err(PreEntryRejection::WrongPlan);
    }
    check_pre_entry(signal, actual_open)
}

impl PlanEvaluator for SessionBreakoutEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::SessionBreakout
    }

    fn algorithm_version(&self) -> &'static str {
        "orb-v1"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::SessionBreakout
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::SessionBreakout(parameters) = &profile.parameters
        else {
            return Err(PlanError::SignalIdentityMismatch);
        };
        let mut next_state = state.clone();
        let mut internal: SessionInternal = if next_state.internal.is_null() {
            SessionInternal::default()
        } else {
            serde_json::from_value(next_state.internal.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?
        };
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
        }
        if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
            next_state.clear_terminal_trade();
            store_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        }
        store_internal(&mut next_state, &internal)?;
        if snapshot.readiness != analysis_domain::Readiness::Ready {
            next_state.readiness = snapshot.readiness;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_some() {
            return manage_active(next_state, snapshot, parameters);
        }
        if !session_breakout_runs_on(profile.key.target_timeframe)
            || parameters.atr_period != RUNTIME_ATR_PERIOD
        {
            next_state.setup_state = SetupState::Neutral;
            return Ok(empty_evaluation(next_state));
        }
        let evaluated = snapshot.evaluated_closed_candle_time;
        let bar_open = evaluated + 1 - profile.key.target_timeframe.duration_ms();
        let day = bar_open.div_euclid(DAY_MS);
        let minute = (bar_open - day * DAY_MS) / MINUTE_MS;
        if internal.last_session_day == Some(day)
            || minute < i64::from(parameters.range_end_minute_utc)
            || minute >= i64::from(parameters.decision_end_minute_utc)
        {
            next_state.setup_state = SetupState::Neutral;
            return Ok(empty_evaluation(next_state));
        }
        let Some((range_fact, range)) = tradable_range(snapshot, parameters, day)? else {
            next_state.setup_state = SetupState::Neutral;
            return Ok(empty_evaluation(next_state));
        };
        let Some((close_fact, close)) = latest_close(snapshot)? else {
            next_state.setup_state = SetupState::Watching;
            return Ok(empty_evaluation(next_state));
        };
        let direction = if close > range.high {
            Direction::Buy
        } else if close < range.low {
            Direction::Sell
        } else {
            next_state.setup_state = SetupState::Watching;
            return Ok(empty_evaluation(next_state));
        };
        let signal = build_signal(
            profile,
            snapshot,
            parameters,
            direction,
            close,
            &range,
            &range_fact,
            &close_fact,
        )?;
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
        internal.last_session_day = Some(day);
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
    internal: &SessionInternal,
) -> Result<(), PlanError> {
    state.internal = serde_json::to_value(internal)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(())
}

/// The day's range when it is complete, built from the configured window, and
/// inside the normalized-width band.
fn tradable_range(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SessionBreakoutParameters,
    day: i64,
) -> Result<Option<(ConfirmedFact<Value>, SessionRangeFact)>, PlanError> {
    let Some(fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::SessionRange)
        .filter_map(|fact| {
            let value: SessionRangeFact = serde_json::from_value(fact.value.clone()).ok()?;
            (value.session_day == day).then_some(fact)
        })
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let range: SessionRangeFact = serde_json::from_value(fact.value.clone())
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    let day_start = day * DAY_MS;
    let window_matches = range.window_start
        == day_start + i64::from(parameters.range_start_minute_utc) * MINUTE_MS
        && range.window_end == day_start + i64::from(parameters.range_end_minute_utc) * MINUTE_MS;
    let Some(width) = range.normalized_width else {
        return Ok(None);
    };
    if !window_matches
        || !range.is_complete()
        || range.atr_at_range.is_none()
        || range.high <= range.low
        || width < parameters.minimum_normalized_width
        || width > parameters.maximum_normalized_width
    {
        return Ok(None);
    }
    Ok(Some((fact.clone(), range)))
}

fn latest_close(
    snapshot: &CanonicalMarketSnapshot,
) -> Result<Option<(ConfirmedFact<Value>, Decimal)>, PlanError> {
    let Some(fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::MarketClose)
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    // The breakout must be the close of the bar being evaluated.
    if fact.id.confirmed_at != snapshot.evaluated_closed_candle_time {
        return Ok(None);
    }
    let value = fact
        .value
        .get("value")
        .cloned()
        .unwrap_or_else(|| fact.value.clone());
    let close: Decimal = serde_json::from_value(value)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(Some((fact.clone(), close)))
}

#[allow(clippy::too_many_arguments)]
fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SessionBreakoutParameters,
    direction: Direction,
    entry: Decimal,
    range: &SessionRangeFact,
    range_fact: &ConfirmedFact<Value>,
    close_fact: &ConfirmedFact<Value>,
) -> Result<SignalDefinition, PlanError> {
    let midpoint = (range.high + range.low) / Decimal::TWO;
    let risk = (entry - midpoint).abs();
    let primary_target = match direction {
        Direction::Buy => entry + risk * parameters.target_rr,
        Direction::Sell => entry - risk * parameters.target_rr,
        Direction::None => return Err(PlanError::SignalIdentityMismatch),
    };
    if risk <= Decimal::ZERO || primary_target <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "session breakout risk levels must remain positive".to_owned(),
        ));
    }
    let risk_reward = ((primary_target - entry).abs() / risk).round_dp(6);
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("orb effective time overflow".to_owned()))?;
    // A next-open entry happens at the next bar's open or not at all.
    let expires_at = effective_at.checked_add(profile.key.target_timeframe.duration_ms());
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"SESSION_BREAKOUT",
        &evaluated.to_be_bytes(),
    ]);
    Ok(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::SessionBreakout,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(entry),
        stop_loss: midpoint,
        primary_target,
        execution_plan: None,
        reference_levels: vec![
            ReferenceLevel {
                kind: "RANGE_HIGH".to_owned(),
                price: range.high,
            },
            ReferenceLevel {
                kind: "RANGE_LOW".to_owned(),
                price: range.low,
            },
            ReferenceLevel {
                kind: "RANGE_MIDPOINT".to_owned(),
                price: midpoint,
            },
        ],
        risk_reward,
        pre_entry_validation: Some(PreEntryValidation {
            minimum_rr: parameters.minimum_entry_rr,
            reject_if_target_crossed: true,
            reject_if_stop_crossed: true,
        }),
        confidence: Decimal::ONE,
        evidence: vec![
            evidence(&range_fact.id, "SESSION_RANGE"),
            evidence(&close_fact.id, "BREAKOUT_CLOSE"),
        ],
        htf_context: None,
        effective_at,
        detected_at: effective_at,
        published_at: effective_at,
        release_id: profile.versions.release_id,
        algorithm_version: profile.versions.algorithm_version.clone(),
        parameter_version: profile.versions.parameter_version.clone(),
        created_at: effective_at,
        expires_at,
    })
}

/// An open trade leaves at the time-exit open of its session day. Unfilled
/// signals expire through `expires_at`.
fn manage_active(
    state: PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SessionBreakoutParameters,
) -> Result<PlanEvaluation, PlanError> {
    if state.trade_state != TradeState::Open {
        return Ok(empty_evaluation(state));
    }
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    let signal_bar_open = signal.evaluated_at + 1 - signal.target_timeframe.duration_ms();
    let exit_open = signal_bar_open.div_euclid(DAY_MS) * DAY_MS
        + i64::from(parameters.time_exit_minute_utc) * MINUTE_MS;
    if evaluated + 1 < exit_open {
        return Ok(empty_evaluation(state));
    }
    let directive = management_directive(
        &state,
        &signal,
        evaluated,
        DirectiveKind::ExitAtNextOpen,
        None,
        "ORB_TIME_EXIT",
    )?;
    Ok(PlanEvaluation {
        next_state: state,
        signal: None,
        events: Vec::new(),
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    })
}
