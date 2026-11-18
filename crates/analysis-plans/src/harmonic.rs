//! Harmonic plans: `HARMONIC` (Crab, `harmonic-crab-limit-v2`),
//! `HARMONIC_CONFIRM` (Crab, `harmonic-crab-confirm-v1`) and `HARMONIC_GB`
//! (Gartley/Bat, `harmonic-gb-limit-v1`, spec
//! `2026-11-18-harmonic-gartley-bat-plan-design.md`).
//!
//! Pre-registered in `docs/superpowers/specs/2026-11-18-harmonic-crab-plan-design.md`
//! and `2026-11-18-harmonic-crab-limit-v2-design.md`: an ATR zigzag over the
//! confirmed swings yields X-A-B-C with Crab ratios and a PRZ centred on the
//! 1.618 extension of XA. The limit plan waits in the PRZ once a close has passed
//! X (its primary target), once per C; the confirm plan enters at the next open
//! after a D swing is confirmed inside the PRZ.

use crate::common::{
    TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact, deterministic_uuid,
    empty_evaluation, evidence, htf_context, immutable_extension_target,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
    shared_trade_state_from_value, store_or_clear_shared_trade_state, store_shared_trade_state,
    typed_facts,
};
use crate::mean_reversion::{PreEntryRejection, check_pre_entry};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::{ConfirmedSwing, SwingKind};
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    HarmonicParameters, HarmonicPattern, ManagementDirective, MaterializedProfile, PlanFeedback,
    PlanKind, PlanParameters, PreEntryValidation, PriceZone, ReferenceLevel, SetupState,
    SignalDefinition, SignalEvent, SignalEventKind, TradeState,
};
use market_domain::Timeframe;
use rust_decimal::Decimal;
use serde_json::Value;

/// Runtime-state key holding the confirmation time of the last C the limit plan
/// signalled, kept beside the shared trade state.
const SIGNALLED_C_KEY: &str = "harmonicSignalledC";

pub struct HarmonicEvaluator;
pub struct HarmonicConfirmEvaluator;
pub struct HarmonicGbEvaluator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryMode {
    /// Crab limit (`HARMONIC`): arms once the close has passed X.
    Limit,
    Confirm,
    /// Gartley/Bat limit (`HARMONIC_GB`): arms once the close has passed the
    /// primary target.
    LimitGb,
}

impl EntryMode {
    const fn plan(self) -> PlanKind {
        match self {
            Self::Limit => PlanKind::Harmonic,
            Self::Confirm => PlanKind::HarmonicConfirm,
            Self::LimitGb => PlanKind::HarmonicGb,
        }
    }

    const fn is_limit(self) -> bool {
        matches!(self, Self::Limit | Self::LimitGb)
    }
}

/// Only the agreed research timeframes run; 1m, 4h and 1d stay silent.
pub const fn harmonic_runs_on(timeframe: Timeframe) -> bool {
    matches!(
        timeframe,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
    )
}

/// Next-open checks for HARMONIC_CONFIRM: cancel when the open is at or through
/// the stop or the target, or when the reward/risk at the open is below the
/// signal's minimum.
pub fn validate_harmonic_confirm_entry(
    signal: &SignalDefinition,
    actual_open: Decimal,
) -> Result<Decimal, PreEntryRejection> {
    if signal.plan != PlanKind::HarmonicConfirm {
        return Err(PreEntryRejection::WrongPlan);
    }
    check_pre_entry(signal, actual_open)
}

impl PlanEvaluator for HarmonicEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Harmonic
    }

    fn algorithm_version(&self) -> &'static str {
        "harmonic-crab-limit-v2"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        evaluate(EntryMode::Limit, profile, snapshot, state, feedback)
    }
}

impl PlanEvaluator for HarmonicGbEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::HarmonicGb
    }

    fn algorithm_version(&self) -> &'static str {
        "harmonic-gb-limit-v1"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        evaluate(EntryMode::LimitGb, profile, snapshot, state, feedback)
    }
}

impl PlanEvaluator for HarmonicConfirmEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::HarmonicConfirm
    }

    fn algorithm_version(&self) -> &'static str {
        "harmonic-crab-confirm-v1"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        evaluate(EntryMode::Confirm, profile, snapshot, state, feedback)
    }
}

#[derive(Clone, Debug)]
struct PatternMatch {
    pattern: HarmonicPattern,
    direction: Direction,
    x: ConfirmedSwing,
    a: ConfirmedSwing,
    b: ConfirmedSwing,
    c: ConfirmedSwing,
    d_xa: Decimal,
    prz_low: Decimal,
    prz_high: Decimal,
}

struct HarmonicSetup {
    pattern: PatternMatch,
    d: Option<ConfirmedSwing>,
    atr: Decimal,
    atr_id: FactId,
    latest_close: Decimal,
}

fn signalled_c(internal: &Value) -> Option<i64> {
    internal.get(SIGNALLED_C_KEY).and_then(Value::as_i64)
}

fn remember_signalled_c(state: &mut PlanRuntimeState, confirmed_at: i64) -> Result<(), PlanError> {
    match &mut state.internal {
        Value::Null => {
            state.internal = serde_json::json!({ SIGNALLED_C_KEY: confirmed_at });
        }
        Value::Object(map) => {
            map.insert(SIGNALLED_C_KEY.to_owned(), Value::from(confirmed_at));
        }
        _ => {
            return Err(PlanError::Serialization(
                "harmonic runtime state is not an object".to_owned(),
            ));
        }
    }
    Ok(())
}

/// The shared trade state rewrites or clears `internal`; the remembered C is put
/// back after every evaluation so one C never signals twice.
fn evaluate(
    mode: EntryMode,
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    state: &PlanRuntimeState,
    feedback: &[PlanFeedback],
) -> Result<PlanEvaluation, PlanError> {
    let remembered = signalled_c(&state.internal);
    let mut evaluation = evaluate_inner(mode, profile, snapshot, state, feedback, remembered)?;
    if let Some(latest) = signalled_c(&evaluation.next_state.internal).max(remembered) {
        remember_signalled_c(&mut evaluation.next_state, latest)?;
    }
    Ok(evaluation)
}

fn evaluate_inner(
    mode: EntryMode,
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    state: &PlanRuntimeState,
    feedback: &[PlanFeedback],
    remembered: Option<i64>,
) -> Result<PlanEvaluation, PlanError> {
    if profile.key.plan != mode.plan() || snapshot.target_timeframe != profile.key.target_timeframe
    {
        return Err(PlanError::SignalIdentityMismatch);
    }
    let parameters = match (&profile.parameters, mode) {
        (PlanParameters::Harmonic(parameters), EntryMode::Limit)
        | (PlanParameters::HarmonicConfirm(parameters), EntryMode::Confirm)
        | (PlanParameters::HarmonicGb(parameters), EntryMode::LimitGb) => parameters,
        _ => return Err(PlanError::SignalIdentityMismatch),
    };
    let mut next_state = state.clone();
    let mut shared = shared_trade_state_from_value(&next_state.internal)?;
    for item in feedback {
        apply_feedback(&mut next_state, item)?;
        apply_shared_trade_feedback(&mut shared, &next_state, item)?;
    }
    if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
        next_state.clear_terminal_trade();
        next_state.internal = serde_json::Value::Null;
        return Ok(empty_evaluation(next_state));
    }
    store_or_clear_shared_trade_state(&mut next_state, &shared)?;
    if snapshot.readiness != analysis_domain::Readiness::Ready {
        next_state.readiness = snapshot.readiness;
        return Ok(empty_evaluation(next_state));
    }
    if next_state.active_signal.is_some() {
        return manage_active(
            mode,
            next_state,
            snapshot,
            parameters,
            shared,
            &profile.profit_protection,
            &profile.position_management,
        );
    }
    if !harmonic_runs_on(profile.key.target_timeframe) || parameters.patterns.is_empty() {
        next_state.setup_state = SetupState::Watching;
        return Ok(empty_evaluation(next_state));
    }
    let Some(setup) = find_setup(mode, snapshot, parameters, remembered)? else {
        next_state.setup_state = SetupState::Watching;
        return Ok(empty_evaluation(next_state));
    };
    let Some(signal) = build_signal(mode, profile, snapshot, parameters, &setup)? else {
        next_state.setup_state = SetupState::Watching;
        return Ok(empty_evaluation(next_state));
    };
    if let CandidateDecision::Rejected(rejected) = create_signal(&mut next_state, signal.clone())? {
        return Ok(PlanEvaluation {
            next_state,
            signal: None,
            events: Vec::new(),
            directives: Vec::new(),
            rejected_candidates: vec![rejected],
        });
    }
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
    if mode.is_limit() {
        remember_signalled_c(&mut next_state, setup.pattern.c.confirmed_at)?;
    }
    Ok(PlanEvaluation {
        next_state,
        signal: Some(signal),
        events: vec![event],
        directives: Vec::new(),
        rejected_candidates: Vec::new(),
    })
}

/// ATR zigzag: a swing of the last pivot's kind replaces it when more extreme;
/// an opposite swing is appended only when it is at least `min_leg` away.
fn zigzag(mut swings: Vec<ConfirmedSwing>, min_leg: Decimal) -> Vec<ConfirmedSwing> {
    swings.sort_by(|left, right| {
        left.pivot_open_time
            .cmp(&right.pivot_open_time)
            .then(left.confirmed_at.cmp(&right.confirmed_at))
            .then(left.id.ordinal.cmp(&right.id.ordinal))
    });
    let mut pivots: Vec<ConfirmedSwing> = Vec::new();
    for swing in swings {
        let Some(last) = pivots.last_mut() else {
            pivots.push(swing);
            continue;
        };
        if last.kind == swing.kind {
            let more_extreme = match swing.kind {
                SwingKind::High => swing.price > last.price,
                SwingKind::Low => swing.price < last.price,
            };
            if more_extreme {
                *last = swing;
            }
        } else if (swing.price - last.price).abs() >= min_leg {
            pivots.push(swing);
        }
    }
    pivots
}

fn ratio(value: i64, scale: u32) -> Decimal {
    Decimal::new(value, scale)
}

/// Registered ratios: AB/XA and BC/AB ranges, the XA level of D
/// (D_xa = A - level x XA, bullish) and the BC projection range of CD.
struct Geometry {
    ab_xa: (Decimal, Decimal),
    bc_ab: (Decimal, Decimal),
    d_level: Decimal,
    cd_bc: (Decimal, Decimal),
}

fn geometry(pattern: HarmonicPattern) -> Geometry {
    let bc_ab = (ratio(382, 3), ratio(886, 3));
    match pattern {
        HarmonicPattern::Crab => Geometry {
            ab_xa: (ratio(382, 3), ratio(618, 3)),
            bc_ab,
            d_level: ratio(1618, 3),
            cd_bc: (ratio(224, 2), ratio(3618, 3)),
        },
        HarmonicPattern::Gartley => Geometry {
            ab_xa: (ratio(618, 3), ratio(618, 3)),
            bc_ab,
            d_level: ratio(786, 3),
            cd_bc: (ratio(1272, 3), ratio(1618, 3)),
        },
        HarmonicPattern::Bat => Geometry {
            ab_xa: (ratio(382, 3), ratio(500, 3)),
            bc_ab,
            d_level: ratio(886, 3),
            cd_bc: (ratio(1618, 3), ratio(2618, 3)),
        },
    }
}

/// The first of `patterns` whose proportions X-A-B-C has.
fn first_match(
    patterns: &[HarmonicPattern],
    x: &ConfirmedSwing,
    a: &ConfirmedSwing,
    b: &ConfirmedSwing,
    c: &ConfirmedSwing,
    tolerance: Decimal,
    prz_half_width: Decimal,
) -> Option<PatternMatch> {
    patterns
        .iter()
        .find_map(|pattern| match_pattern(*pattern, x, a, b, c, tolerance, prz_half_width))
}

/// Pattern proportions on X-A-B-C (bullish: X low, A high, B low, C high).
fn match_pattern(
    pattern: HarmonicPattern,
    x: &ConfirmedSwing,
    a: &ConfirmedSwing,
    b: &ConfirmedSwing,
    c: &ConfirmedSwing,
    tolerance: Decimal,
    prz_half_width: Decimal,
) -> Option<PatternMatch> {
    let direction = match (x.kind, a.kind, b.kind, c.kind) {
        (SwingKind::Low, SwingKind::High, SwingKind::Low, SwingKind::High) => Direction::Buy,
        (SwingKind::High, SwingKind::Low, SwingKind::High, SwingKind::Low) => Direction::Sell,
        _ => return None,
    };
    let sign = if direction == Direction::Buy {
        Decimal::ONE
    } else {
        -Decimal::ONE
    };
    let xa = (a.price - x.price) * sign;
    let ab = (a.price - b.price) * sign;
    let bc = (c.price - b.price) * sign;
    if xa <= Decimal::ZERO || ab <= Decimal::ZERO || bc <= Decimal::ZERO {
        return None;
    }
    let lower = Decimal::ONE - tolerance;
    let upper = Decimal::ONE + tolerance;
    let within =
        |value: Decimal, low: Decimal, high: Decimal| value >= low * lower && value <= high * upper;
    let rules = geometry(pattern);
    if !within(ab / xa, rules.ab_xa.0, rules.ab_xa.1)
        || !within(bc / ab, rules.bc_ab.0, rules.bc_ab.1)
    {
        return None;
    }
    let d_xa = a.price - sign * rules.d_level * xa;
    let cd = (c.price - d_xa) * sign;
    if !within(cd, rules.cd_bc.0 * bc, rules.cd_bc.1 * bc) {
        return None;
    }
    Some(PatternMatch {
        pattern,
        direction,
        x: x.clone(),
        a: a.clone(),
        b: b.clone(),
        c: c.clone(),
        d_xa,
        prz_low: d_xa - prz_half_width,
        prz_high: d_xa + prz_half_width,
    })
}

/// D_ref + 0.382 x AD toward A.
fn primary_target(direction: Direction, d_ref: Decimal, a: Decimal) -> Decimal {
    let sign = if direction == Direction::Sell {
        -Decimal::ONE
    } else {
        Decimal::ONE
    };
    d_ref + sign * ratio(382, 3) * (a - d_ref).abs()
}

fn find_setup(
    mode: EntryMode,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &HarmonicParameters,
    remembered: Option<i64>,
) -> Result<Option<HarmonicSetup>, PlanError> {
    let Some(atr) = decimal_fact(snapshot, FactKind::Atr)? else {
        return Ok(None);
    };
    if atr <= Decimal::ZERO {
        return Ok(None);
    }
    let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
        return Ok(None);
    };
    let Some(atr_id) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Atr)
        .max_by_key(|fact| fact.id.confirmed_at)
        .map(|fact| fact.id.clone())
    else {
        return Ok(None);
    };
    let mut swings = typed_facts::<ConfirmedSwing>(&snapshot.target.facts, FactKind::SwingHigh)?;
    swings.extend(typed_facts::<ConfirmedSwing>(
        &snapshot.target.facts,
        FactKind::SwingLow,
    )?);
    let pivots = zigzag(swings, atr * parameters.min_leg_atr);
    let evaluated = snapshot.evaluated_closed_candle_time;
    let prz_half_width = atr * parameters.prz_half_width_atr;
    let setup = match mode {
        EntryMode::Limit | EntryMode::LimitGb => {
            let [.., x, a, b, c] = pivots.as_slice() else {
                return Ok(None);
            };
            let arm_window = snapshot
                .target_timeframe
                .duration_ms()
                .checked_mul(i64::from(parameters.max_bars_valid))
                .ok_or_else(|| {
                    PlanError::Serialization("harmonic arm window overflow".to_owned())
                })?;
            if remembered.is_some_and(|time| time >= c.confirmed_at)
                || evaluated < c.confirmed_at
                || evaluated - c.confirmed_at > arm_window
            {
                return Ok(None);
            }
            let Some(pattern) = first_match(
                &parameters.patterns,
                x,
                a,
                b,
                c,
                parameters.ratio_tolerance,
                prz_half_width,
            ) else {
                return Ok(None);
            };
            // Arm only once the close has passed the primary target and is
            // still short of the PRZ. The Crab (v2) registered X as that level;
            // its target D_xa + 0.382 x AD equals X up to the rounding of 1.618.
            let arm_level = if pattern.pattern == HarmonicPattern::Crab {
                x.price
            } else {
                primary_target(pattern.direction, pattern.d_xa, pattern.a.price)
            };
            let armed = match pattern.direction {
                Direction::Buy => latest_close <= arm_level && latest_close > pattern.prz_high,
                Direction::Sell => latest_close >= arm_level && latest_close < pattern.prz_low,
                Direction::None => false,
            };
            if !armed {
                return Ok(None);
            }
            HarmonicSetup {
                pattern,
                d: None,
                atr,
                atr_id,
                latest_close,
            }
        }
        EntryMode::Confirm => {
            let [.., x, a, b, c, d] = pivots.as_slice() else {
                return Ok(None);
            };
            if d.confirmed_at != evaluated {
                return Ok(None);
            }
            let Some(pattern) = first_match(
                &parameters.patterns,
                x,
                a,
                b,
                c,
                parameters.ratio_tolerance,
                prz_half_width,
            ) else {
                return Ok(None);
            };
            if d.price < pattern.prz_low || d.price > pattern.prz_high {
                return Ok(None);
            }
            HarmonicSetup {
                pattern,
                d: Some(d.clone()),
                atr,
                atr_id,
                latest_close,
            }
        }
    };
    Ok(Some(setup))
}

fn build_signal(
    mode: EntryMode,
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &HarmonicParameters,
    setup: &HarmonicSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let pattern = &setup.pattern;
    let direction = pattern.direction;
    let sign = match direction {
        Direction::Buy => Decimal::ONE,
        Direction::Sell => -Decimal::ONE,
        Direction::None => return Ok(None),
    };
    let outer_edge = if direction == Direction::Buy {
        pattern.prz_low
    } else {
        pattern.prz_high
    };
    let (d_ref, thesis, entry_reference, zone_width) = match (mode, &setup.d) {
        (EntryMode::Limit | EntryMode::LimitGb, _) => (
            pattern.d_xa,
            outer_edge,
            pattern.d_xa,
            pattern.prz_high - pattern.prz_low,
        ),
        (EntryMode::Confirm, Some(d)) => (d.price, d.price, setup.latest_close, Decimal::ZERO),
        (EntryMode::Confirm, None) => return Ok(None),
    };
    let leg = (pattern.a.price - d_ref).abs();
    let target = primary_target(direction, d_ref, pattern.a.price);
    let extension = d_ref + sign * ratio(618, 3) * leg;
    if (target - entry_reference) * sign <= Decimal::ZERO {
        return Ok(None);
    }
    let Some(execution_plan) = materialize_execution_plan(
        direction,
        entry_reference,
        thesis,
        target,
        setup.atr,
        parameters.stop_buffer_atr,
        zone_width,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let stop_loss = execution_plan.hard_stop;
    let risk = (entry_reference - stop_loss).abs();
    let reward = (target - entry_reference) * sign;
    if risk <= Decimal::ZERO || reward / risk < parameters.minimum_rr {
        return Ok(None);
    }
    let plan = mode.plan();
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("harmonic effective time overflow".to_owned()))?;
    let expires_at = effective_at.checked_add(
        profile
            .key
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_bars_valid))
            .ok_or_else(|| PlanError::Serialization("harmonic expiry overflow".to_owned()))?,
    );
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        plan.as_str().as_bytes(),
        &evaluated.to_be_bytes(),
    ]);
    let mut points = vec![
        ("HARMONIC_X", &pattern.x),
        ("HARMONIC_A", &pattern.a),
        ("HARMONIC_B", &pattern.b),
        ("HARMONIC_C", &pattern.c),
    ];
    if let Some(d) = &setup.d {
        points.push(("HARMONIC_D", d));
    }
    let mut reference_levels: Vec<_> = points
        .iter()
        .map(|(kind, swing)| ReferenceLevel {
            kind: (*kind).to_owned(),
            price: swing.price,
        })
        .collect();
    reference_levels.extend([
        ReferenceLevel {
            kind: "PRZ_LOW".to_owned(),
            price: pattern.prz_low,
        },
        ReferenceLevel {
            kind: "PRZ_HIGH".to_owned(),
            price: pattern.prz_high,
        },
        ReferenceLevel {
            kind: "TP2_EXTENSION_TARGET".to_owned(),
            price: extension,
        },
    ]);
    let mut evidence_refs: Vec<_> = points
        .iter()
        .map(|(kind, swing)| evidence(&swing.id, kind))
        .collect();
    evidence_refs.push(evidence(&setup.atr_id, "ATR_BUFFER"));
    let (execution_instruction, entry_zone, projected_entry, pre_entry_validation) = match mode {
        EntryMode::Limit | EntryMode::LimitGb => (
            ExecutionInstruction::LimitZone,
            Some(
                PriceZone::new(pattern.prz_low, pattern.prz_high)
                    .map_err(|error| PlanError::Serialization(error.to_string()))?,
            ),
            None,
            None,
        ),
        EntryMode::Confirm => (
            ExecutionInstruction::NextOpen,
            None,
            Some(entry_reference),
            Some(PreEntryValidation {
                minimum_rr: parameters.minimum_entry_rr,
                reject_if_target_crossed: true,
                reject_if_stop_crossed: true,
            }),
        ),
    };
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction,
        execution_instruction,
        entry_zone,
        projected_entry,
        stop_loss,
        primary_target: target,
        execution_plan: Some(execution_plan),
        reference_levels,
        risk_reward: (reward / risk).round_dp(6),
        pre_entry_validation,
        confidence: Decimal::ONE,
        evidence: evidence_refs,
        htf_context: Some(htf_context(snapshot, direction, None)),
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

fn reference_price(signal: &SignalDefinition, kind: &str) -> Option<Decimal> {
    signal
        .reference_levels
        .iter()
        .find(|level| level.kind == kind)
        .map(|level| level.price)
}

fn manage_active(
    mode: EntryMode,
    mut state: PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &HarmonicParameters,
    mut shared: crate::common::SharedTradeState,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    if state.trade_state == TradeState::NotEntered {
        // The next-open entry of the confirm plan is handled by the tracker.
        if mode == EntryMode::Confirm {
            return Ok(empty_evaluation(state));
        }
        let expired = signal.expires_at.is_some_and(|expiry| evaluated >= expiry);
        let c_exceeded = match (
            decimal_fact(snapshot, FactKind::MarketClose)?,
            reference_price(&signal, "HARMONIC_C"),
        ) {
            (Some(close), Some(c)) => match signal.direction {
                Direction::Buy => close > c,
                Direction::Sell => close < c,
                Direction::None => false,
            },
            _ => false,
        };
        let (kind, event_kind, reason) = if expired {
            (
                DirectiveKind::ExpireEntry,
                SignalEventKind::Expired,
                "HARMONIC_ENTRY_EXPIRED",
            )
        } else if c_exceeded {
            (
                DirectiveKind::CancelEntry,
                SignalEventKind::Invalidated,
                "HARMONIC_C_EXCEEDED",
            )
        } else {
            return Ok(empty_evaluation(state));
        };
        return entry_directive(state, &signal, evaluated, kind, event_kind, reason);
    }
    if let Some(directive) = shared_thesis_invalidation_directive(&mut state, snapshot)? {
        store_shared_trade_state(&mut state, &shared)?;
        return Ok(directive_only(state, directive));
    }
    if let Some(directive) = shared_profit_protection_directive(
        &mut state,
        &mut shared,
        snapshot,
        profit_protection,
        position_management,
    )? {
        store_shared_trade_state(&mut state, &shared)?;
        return Ok(directive_only(state, directive));
    }
    if let Some(directive) = shared_target_extension_directive(
        &mut state,
        &mut shared,
        snapshot,
        TargetExtensionRequest {
            plan_supplied_target: immutable_extension_target(&signal),
            position_management,
            minimum_rr: parameters.minimum_rr,
        },
    )? {
        store_shared_trade_state(&mut state, &shared)?;
        return Ok(directive_only(state, directive));
    }
    store_shared_trade_state(&mut state, &shared)?;
    Ok(empty_evaluation(state))
}

fn directive_only(state: PlanRuntimeState, directive: ManagementDirective) -> PlanEvaluation {
    PlanEvaluation {
        next_state: state,
        signal: None,
        events: Vec::new(),
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    }
}

fn entry_directive(
    mut state: PlanRuntimeState,
    signal: &SignalDefinition,
    evaluated: i64,
    kind: DirectiveKind,
    event_kind: SignalEventKind,
    reason: &str,
) -> Result<PlanEvaluation, PlanError> {
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("harmonic directive time overflow".to_owned()))?;
    let directive = ManagementDirective {
        directive_id: deterministic_uuid(&[
            signal.signal_id.as_bytes(),
            kind.as_str().as_bytes(),
            reason.as_bytes(),
            &evaluated.to_be_bytes(),
        ]),
        signal_id: signal.signal_id,
        virtual_trade_id: state.active_virtual_trade_id,
        kind,
        effective_at,
        price: None,
        reason: reason.to_owned(),
        source_closed_candle_time: evaluated,
        algorithm_version: signal.algorithm_version.clone(),
        parameter_version: signal.parameter_version.clone(),
        reverse_signal: None,
    };
    let event = SignalEvent {
        event_id: deterministic_uuid(&[
            signal.signal_id.as_bytes(),
            event_kind.as_str().as_bytes(),
            &evaluated.to_be_bytes(),
        ]),
        signal_id: signal.signal_id,
        sequence: state.next_event_sequence,
        kind: event_kind,
        effective_at,
        recorded_at: effective_at,
        virtual_trade_id: state.active_virtual_trade_id,
        price: None,
        reason: Some(reason.to_owned()),
    };
    append_event(&mut state, &event)?;
    Ok(PlanEvaluation {
        next_state: state,
        signal: None,
        events: vec![event],
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis_domain::FactId;
    use market_domain::Symbol;

    fn swing(kind: SwingKind, ordinal: u32, time: i64, price: i64) -> ConfirmedSwing {
        let fact_kind = if kind == SwingKind::High {
            FactKind::SwingHigh
        } else {
            FactKind::SwingLow
        };
        ConfirmedSwing {
            id: FactId {
                symbol: Symbol::new("XAUUSDT").unwrap(),
                timeframe: Timeframe::M15,
                kind: fact_kind,
                confirmed_at: time + 2,
                ordinal,
                algorithm_version: "fixture-v1".to_owned(),
                parameter_version: "fixture-v1".to_owned(),
            },
            kind,
            label: None,
            price: Decimal::new(price, 0),
            pivot_open_time: time,
            confirmed_at: time + 2,
            source_start_time: time,
            source_end_time: time + 2,
        }
    }

    /// Bullish Crab: X 1000, A 1100 (XA 100), B 1050 (AB/XA 0.5), C 1095
    /// (BC/AB 0.9 <= 0.886 x 1.05), D_xa = 1100 - 161.8 = 938.2,
    /// CD 156.8 = 3.484 x BC, inside 2.24 x 0.95 .. 3.618 x 1.05.
    fn bull() -> [ConfirmedSwing; 4] {
        [
            swing(SwingKind::Low, 1, 10, 1000),
            swing(SwingKind::High, 2, 20, 1100),
            swing(SwingKind::Low, 3, 30, 1050),
            swing(SwingKind::High, 4, 40, 1095),
        ]
    }

    fn mirror(swings: &[ConfirmedSwing; 4]) -> [ConfirmedSwing; 4] {
        swings.clone().map(|mut item| {
            item.kind = if item.kind == SwingKind::High {
                SwingKind::Low
            } else {
                SwingKind::High
            };
            item.price = Decimal::new(2000, 0) - item.price;
            item
        })
    }

    fn tolerance() -> Decimal {
        Decimal::new(5, 2)
    }

    #[test]
    fn bullish_and_bearish_crab_pass_with_prz_on_the_xa_extension() {
        let [x, a, b, c] = bull();
        let pattern = match_pattern(
            HarmonicPattern::Crab,
            &x,
            &a,
            &b,
            &c,
            tolerance(),
            Decimal::ONE,
        )
        .expect("bull crab");
        assert_eq!(pattern.direction, Direction::Buy);
        assert_eq!(pattern.d_xa, Decimal::new(9382, 1));
        assert_eq!(pattern.prz_low, Decimal::new(9372, 1));
        assert_eq!(pattern.prz_high, Decimal::new(9392, 1));
        let [x, a, b, c] = mirror(&bull());
        let pattern = match_pattern(
            HarmonicPattern::Crab,
            &x,
            &a,
            &b,
            &c,
            tolerance(),
            Decimal::ONE,
        )
        .expect("bear crab");
        assert_eq!(pattern.direction, Direction::Sell);
        assert_eq!(pattern.d_xa, Decimal::new(10618, 1));
    }

    #[test]
    fn ratios_just_outside_the_tolerated_bounds_fail() {
        let [x, a, _, _] = bull();
        // AB/XA = 0.65 > 0.618 * 1.05 = 0.6489.
        let b = swing(SwingKind::Low, 3, 30, 1035);
        let c = swing(SwingKind::High, 4, 40, 1070);
        assert!(
            match_pattern(
                HarmonicPattern::Crab,
                &x,
                &a,
                &b,
                &c,
                tolerance(),
                Decimal::ONE
            )
            .is_none()
        );
        // AB/XA = 0.36 < 0.382 * 0.95 = 0.3629.
        let b = swing(SwingKind::Low, 3, 30, 1064);
        let c = swing(SwingKind::High, 4, 40, 1080);
        assert!(
            match_pattern(
                HarmonicPattern::Crab,
                &x,
                &a,
                &b,
                &c,
                tolerance(),
                Decimal::ONE
            )
            .is_none()
        );
        // BC/AB = 0.94 > 0.886 * 1.05 = 0.9303.
        let b = swing(SwingKind::Low, 3, 30, 1050);
        let c = swing(SwingKind::High, 4, 40, 1097);
        assert!(
            match_pattern(
                HarmonicPattern::Crab,
                &x,
                &a,
                &b,
                &c,
                tolerance(),
                Decimal::ONE
            )
            .is_none()
        );
    }

    #[test]
    fn xa_extension_outside_the_bc_projection_fails() {
        let [x, a, b, _] = bull();
        // BC 35 (BC/AB 0.7): CD 146.8 = 4.19 x BC > 3.618 x 1.05.
        let c = swing(SwingKind::High, 4, 40, 1085);
        assert!(
            match_pattern(
                HarmonicPattern::Crab,
                &x,
                &a,
                &b,
                &c,
                tolerance(),
                Decimal::ONE
            )
            .is_none()
        );
    }

    #[test]
    fn wrong_alternation_fails() {
        let [x, a, b, c] = bull();
        assert!(
            match_pattern(
                HarmonicPattern::Crab,
                &a,
                &x,
                &b,
                &c,
                tolerance(),
                Decimal::ONE
            )
            .is_none()
        );
    }

    fn pivots(prices: [i64; 4]) -> [ConfirmedSwing; 4] {
        [
            swing(SwingKind::Low, 1, 10, prices[0]),
            swing(SwingKind::High, 2, 20, prices[1]),
            swing(SwingKind::Low, 3, 30, prices[2]),
            swing(SwingKind::High, 4, 40, prices[3]),
        ]
    }

    fn matches(pattern: HarmonicPattern, swings: &[ConfirmedSwing; 4]) -> Option<PatternMatch> {
        let [x, a, b, c] = swings;
        match_pattern(pattern, x, a, b, c, tolerance(), Decimal::ONE)
    }

    #[test]
    fn gartley_and_bat_pass_with_d_on_their_xa_retracement() {
        // Gartley: AB/XA 0.618, BC/AB 0.676, CD 58.6 = 1.402 x BC.
        let gartley = pivots([1000, 1100, 1038, 1080]);
        let found = matches(HarmonicPattern::Gartley, &gartley).expect("bull gartley");
        assert_eq!(found.direction, Direction::Buy);
        assert_eq!(found.d_xa, Decimal::new(10214, 1));
        let found = matches(HarmonicPattern::Gartley, &mirror(&gartley)).expect("bear gartley");
        assert_eq!(found.direction, Direction::Sell);
        assert_eq!(found.d_xa, Decimal::new(9786, 1));
        // Bat: AB/XA 0.45, BC/AB 0.778, CD 78.6 = 2.246 x BC.
        let bat = pivots([1000, 1100, 1055, 1090]);
        let found = matches(HarmonicPattern::Bat, &bat).expect("bull bat");
        assert_eq!(found.d_xa, Decimal::new(10114, 1));
        assert!(matches(HarmonicPattern::Bat, &mirror(&bat)).is_some());
        // The AB/XA ranges are disjoint: neither is the other.
        assert!(matches(HarmonicPattern::Bat, &gartley).is_none());
        assert!(matches(HarmonicPattern::Gartley, &bat).is_none());
    }

    #[test]
    fn gartley_and_bat_bounds_fail_just_outside_tolerance() {
        // Gartley AB/XA 0.58 < 0.618 x 0.95 = 0.5871 (and > Bat 0.525).
        assert!(matches(HarmonicPattern::Gartley, &pivots([1000, 1100, 1042, 1080])).is_none());
        // Gartley AB/XA 0.65 > 0.6489.
        assert!(matches(HarmonicPattern::Gartley, &pivots([1000, 1100, 1035, 1080])).is_none());
        // Gartley BC/AB 0.371 passes, CD 39.6 / BC 23 = 1.72 > 1.618 x 1.05 = 1.699.
        assert!(matches(HarmonicPattern::Gartley, &pivots([1000, 1100, 1038, 1061])).is_none());
        // Bat AB/XA 0.53 > 0.5 x 1.05 = 0.525.
        assert!(matches(HarmonicPattern::Bat, &pivots([1000, 1100, 1047, 1090])).is_none());
        // Bat AB/XA 0.36 < 0.382 x 0.95 = 0.3629.
        assert!(matches(HarmonicPattern::Bat, &pivots([1000, 1100, 1064, 1095])).is_none());
        // Bat BC/AB 0.4 passes, CD 64.6 / BC 16 = 4.04 > 2.618 x 1.05 = 2.749.
        assert!(matches(HarmonicPattern::Bat, &pivots([1000, 1100, 1060, 1076])).is_none());
    }

    #[test]
    fn a_crab_with_ab_xa_between_bat_and_gartley_matches_neither() {
        // AB/XA 0.55: Crab only.
        let crab_only = pivots([1000, 1100, 1045, 1089]);
        assert!(matches(HarmonicPattern::Crab, &crab_only).is_some());
        assert!(
            first_match(
                &[HarmonicPattern::Gartley, HarmonicPattern::Bat],
                &crab_only[0],
                &crab_only[1],
                &crab_only[2],
                &crab_only[3],
                tolerance(),
                Decimal::ONE,
            )
            .is_none()
        );
    }

    #[test]
    fn zigzag_replaces_same_kind_and_ignores_small_reversals() {
        let swings = vec![
            swing(SwingKind::Low, 1, 10, 1000),
            swing(SwingKind::High, 2, 20, 1050),
            // Small pullback, ignored.
            swing(SwingKind::Low, 3, 30, 1045),
            // Higher high replaces the previous high.
            swing(SwingKind::High, 4, 40, 1100),
            // Lower low than the first pivot is appended after the high.
            swing(SwingKind::Low, 5, 50, 990),
        ];
        let pivots = zigzag(swings, Decimal::new(20, 0));
        let prices: Vec<_> = pivots.iter().map(|pivot| pivot.price).collect();
        assert_eq!(
            prices,
            vec![
                Decimal::new(1000, 0),
                Decimal::new(1100, 0),
                Decimal::new(990, 0)
            ]
        );
    }
}
