use crate::common::{
    SharedTradeState, TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact,
    deterministic_uuid, empty_evaluation, evidence, htf_context, immutable_extension_target,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    ManagementDirective, MeanReversionParameters, PlanFeedback, PlanKind, PreEntryValidation,
    ReferenceLevel, SetupState, SignalDefinition, SignalEvent, SignalEventKind, TradeState,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub struct MeanReversionEvaluator;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BandValue {
    period: usize,
    deviation_multiplier: Decimal,
    upper: Decimal,
    middle: Decimal,
    lower: Decimal,
    deviation: Decimal,
    bandwidth: Decimal,
    zscore: Decimal,
    close: Decimal,
    low: Decimal,
    high: Decimal,
}

#[derive(Clone)]
struct BandObservation {
    id: FactId,
    value: BandValue,
}

struct MeanSetup {
    direction: Direction,
    first: BandObservation,
    last_extreme: BandObservation,
    confirmation: BandObservation,
    accumulated_extreme: Decimal,
    atr_id: FactId,
    atr: Decimal,
    adx_id: FactId,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeanInternal {
    consumed_through: Option<i64>,
    #[serde(default)]
    shared_trade: SharedTradeState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreEntryRejection {
    TargetCrossed,
    StopCrossed,
    InsufficientRiskReward,
    MissingValidation,
    WrongPlan,
}

pub fn validate_mean_reversion_entry(
    signal: &SignalDefinition,
    actual_open: Decimal,
) -> Result<Decimal, PreEntryRejection> {
    if signal.plan != PlanKind::MeanReversion {
        return Err(PreEntryRejection::WrongPlan);
    }
    check_pre_entry(signal, actual_open)
}

/// The next-open checks carried by `pre_entry_validation`, shared by every plan
/// that publishes one. Callers check the plan first.
pub(crate) fn check_pre_entry(
    signal: &SignalDefinition,
    actual_open: Decimal,
) -> Result<Decimal, PreEntryRejection> {
    let Some(validation) = &signal.pre_entry_validation else {
        return Err(PreEntryRejection::MissingValidation);
    };
    let (risk, reward, target_crossed, stop_crossed) = match signal.direction {
        Direction::Buy => (
            actual_open - signal.stop_loss,
            signal.primary_target - actual_open,
            actual_open >= signal.primary_target,
            actual_open <= signal.stop_loss,
        ),
        Direction::Sell => (
            signal.stop_loss - actual_open,
            actual_open - signal.primary_target,
            actual_open <= signal.primary_target,
            actual_open >= signal.stop_loss,
        ),
        Direction::None => return Err(PreEntryRejection::WrongPlan),
    };
    if validation.reject_if_target_crossed && target_crossed {
        return Err(PreEntryRejection::TargetCrossed);
    }
    if validation.reject_if_stop_crossed && stop_crossed {
        return Err(PreEntryRejection::StopCrossed);
    }
    if risk <= Decimal::ZERO || reward <= Decimal::ZERO {
        return Err(PreEntryRejection::InsufficientRiskReward);
    }
    let rr = (reward / risk).round_dp(6);
    if rr < validation.minimum_rr {
        return Err(PreEntryRejection::InsufficientRiskReward);
    }
    Ok(rr)
}

impl PlanEvaluator for MeanReversionEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::MeanReversion
    }

    fn algorithm_version(&self) -> &'static str {
        "mean-v3"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::MeanReversion
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::MeanReversion(parameters) = &profile.parameters else {
            return Err(PlanError::SignalIdentityMismatch);
        };
        let mut next_state = state.clone();
        let mut internal: MeanInternal = if next_state.internal.is_null() {
            MeanInternal::default()
        } else {
            serde_json::from_value(next_state.internal.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?
        };
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
            apply_shared_trade_feedback(&mut internal.shared_trade, &next_state, item)?;
        }
        if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
            internal.consumed_through = next_state
                .active_signal
                .as_ref()
                .map(|signal| signal.evaluated_at);
            internal.shared_trade = SharedTradeState::default();
            next_state.clear_terminal_trade();
            next_state.internal = serde_json::to_value(internal)
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            return Ok(empty_evaluation(next_state));
        }
        next_state.internal = serde_json::to_value(&internal)
            .map_err(|error| PlanError::Serialization(error.to_string()))?;
        if snapshot.readiness != analysis_domain::Readiness::Ready {
            next_state.readiness = snapshot.readiness;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_some() {
            return manage_active(
                next_state,
                snapshot,
                parameters,
                internal,
                &profile.profit_protection,
                &profile.position_management,
            );
        }
        let Some(setup) = find_setup(snapshot, parameters, internal.consumed_through)? else {
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

fn observations(
    snapshot: &CanonicalMarketSnapshot,
    consumed_through: Option<i64>,
) -> Result<Vec<BandObservation>, PlanError> {
    snapshot
        .target
        .facts
        .iter()
        .filter(|fact| {
            fact.id.kind == FactKind::Bollinger
                && consumed_through.is_none_or(|cutoff| fact.id.confirmed_at > cutoff)
        })
        .map(|fact| {
            let value = serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok(BandObservation {
                id: fact.id.clone(),
                value,
            })
        })
        .collect()
}

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &MeanReversionParameters,
    consumed_through: Option<i64>,
) -> Result<Option<MeanSetup>, PlanError> {
    let observations = observations(snapshot, consumed_through)?;
    let Some(confirmation) = observations.last() else {
        return Ok(None);
    };
    if confirmation.value.period != usize::try_from(parameters.bollinger_period).unwrap_or_default()
        || confirmation.value.deviation_multiplier != parameters.bollinger_deviation
        || confirmation.value.deviation < Decimal::ZERO
    {
        return Ok(None);
    }
    let before = &observations[..observations.len().saturating_sub(1)];
    let candidates = [
        (
            Direction::Buy,
            before
                .iter()
                .rev()
                .take_while(|item| item.value.close < item.value.lower)
                .cloned()
                .collect::<Vec<_>>(),
        ),
        (
            Direction::Sell,
            before
                .iter()
                .rev()
                .take_while(|item| item.value.close > item.value.upper)
                .cloned()
                .collect::<Vec<_>>(),
        ),
    ];
    for (direction, mut sequence) in candidates {
        if sequence.is_empty() {
            continue;
        }
        sequence.reverse();
        let extreme_index = sequence.iter().position(|item| {
            if direction == Direction::Buy {
                item.value.zscore <= -parameters.extreme_zscore
            } else {
                item.value.zscore >= parameters.extreme_zscore
            }
        });
        let Some(extreme_index) = extreme_index else {
            continue;
        };
        let sequence = &sequence[extreme_index..];
        let reentered = if direction == Direction::Buy {
            confirmation.value.close >= confirmation.value.lower
        } else {
            confirmation.value.close <= confirmation.value.upper
        };
        if !reentered {
            continue;
        }
        let first = sequence.first().expect("sequence is non-empty").clone();
        let last_extreme = sequence.last().expect("sequence is non-empty").clone();
        let max_age = snapshot
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_bars_valid))
            .ok_or_else(|| PlanError::Serialization("mean setup expiry overflow".to_owned()))?;
        if confirmation.id.confirmed_at - first.id.confirmed_at > max_age {
            continue;
        }
        if confirmation.value.bandwidth > parameters.maximum_bandwidth {
            continue;
        }
        let Some(adx_fact) = snapshot
            .target
            .facts
            .iter()
            .filter(|fact| fact.id.kind == FactKind::Adx)
            .max_by_key(|fact| fact.id.confirmed_at)
        else {
            continue;
        };
        let adx: Decimal = serde_json::from_value(
            adx_fact
                .value
                .get("value")
                .cloned()
                .unwrap_or_else(|| adx_fact.value.clone()),
        )
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
        if adx > parameters.maximum_adx {
            continue;
        }
        let Some(atr) = decimal_fact(snapshot, FactKind::Atr)? else {
            continue;
        };
        let Some(atr_id) = snapshot
            .target
            .facts
            .iter()
            .filter(|fact| fact.id.kind == FactKind::Atr)
            .max_by_key(|fact| fact.id.confirmed_at)
            .map(|fact| fact.id.clone())
        else {
            continue;
        };
        let accumulated_extreme = if direction == Direction::Buy {
            sequence
                .iter()
                .map(|item| item.value.low)
                .min()
                .expect("sequence is non-empty")
        } else {
            sequence
                .iter()
                .map(|item| item.value.high)
                .max()
                .expect("sequence is non-empty")
        };
        let stop = if direction == Direction::Buy {
            accumulated_extreme - atr * parameters.atr_buffer_multiplier
        } else {
            accumulated_extreme + atr * parameters.atr_buffer_multiplier
        };
        let risk = (confirmation.value.close - stop).abs();
        let reward = (confirmation.value.middle - confirmation.value.close).abs();
        let target_valid = if direction == Direction::Buy {
            confirmation.value.middle > confirmation.value.close
        } else {
            confirmation.value.middle < confirmation.value.close
        };
        if !target_valid || risk <= Decimal::ZERO {
            continue;
        }
        let projected_rr = (reward / risk).round_dp(6);
        if projected_rr < parameters.minimum_rr {
            continue;
        }
        return Ok(Some(MeanSetup {
            direction,
            first,
            last_extreme,
            confirmation: confirmation.clone(),
            accumulated_extreme,
            atr_id,
            atr,
            adx_id: adx_fact.id.clone(),
        }));
    }
    Ok(None)
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &MeanReversionParameters,
    setup: &MeanSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let entry = setup.confirmation.value.close;
    let analytical_target = setup.confirmation.value.middle;
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        entry,
        setup.accumulated_extreme,
        analytical_target,
        setup.atr,
        parameters.atr_buffer_multiplier,
        Decimal::ZERO,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let stop_loss = execution_plan.hard_stop;
    let final_risk = (entry - stop_loss).abs();
    if final_risk <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "mean final risk must remain positive".to_owned(),
        ));
    }
    let risk_reward = ((analytical_target - entry).abs() / final_risk).round_dp(6);
    if risk_reward < parameters.minimum_rr {
        return Ok(None);
    }
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("mean effective time overflow".to_owned()))?;
    let expires_at = effective_at.checked_add(profile.key.target_timeframe.duration_ms());
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"MEAN_REVERSION",
        &evaluated.to_be_bytes(),
    ]);
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::MeanReversion,
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
                kind: "SETUP_EXTREME".to_owned(),
                price: setup.accumulated_extreme,
            },
            ReferenceLevel {
                kind: "TP2_EXTENSION_TARGET".to_owned(),
                price: match setup.direction {
                    Direction::Buy => setup.confirmation.value.upper,
                    Direction::Sell => setup.confirmation.value.lower,
                    Direction::None => return Ok(None),
                },
            },
        ],
        risk_reward,
        pre_entry_validation: Some(PreEntryValidation {
            minimum_rr: parameters.minimum_rr,
            reject_if_target_crossed: true,
            reject_if_stop_crossed: true,
        }),
        confidence: Decimal::ONE,
        evidence: vec![
            evidence(&setup.first.id, "EXTREME_SEQUENCE_START"),
            evidence(&setup.last_extreme.id, "ACCUMULATED_EXTREME"),
            evidence(&setup.confirmation.id, "BAND_REENTRY"),
            evidence(&setup.adx_id, "RANGE_ADX"),
            evidence(&setup.atr_id, "ATR_BUFFER"),
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
    snapshot: &CanonicalMarketSnapshot,
    parameters: &MeanReversionParameters,
    mut internal: MeanInternal,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    if state.trade_state != TradeState::Open {
        return Ok(empty_evaluation(state));
    }
    let latest_band = observations(snapshot, None)?
        .into_iter()
        .max_by_key(|item| item.id.confirmed_at);
    let latest_adx = decimal_fact(snapshot, FactKind::Adx)?;
    let regime_invalid = latest_band
        .as_ref()
        .is_some_and(|band| band.value.bandwidth > parameters.maximum_bandwidth)
        || latest_adx.is_some_and(|adx| adx > parameters.maximum_adx);
    if !regime_invalid {
        if let Some(directive) = shared_thesis_invalidation_directive(&mut state, snapshot)? {
            state.internal = serde_json::to_value(&internal)
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: vec![directive],
                rejected_candidates: Vec::new(),
            });
        }
        if let Some(directive) = shared_profit_protection_directive(
            &mut state,
            &mut internal.shared_trade,
            snapshot,
            profit_protection,
            position_management,
        )? {
            state.internal = serde_json::to_value(&internal)
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: vec![directive],
                rejected_candidates: Vec::new(),
            });
        }
        let extension_target = immutable_extension_target(
            state
                .active_signal
                .as_ref()
                .ok_or(PlanError::NoActiveSignal)?,
        );
        if let Some(directive) = shared_target_extension_directive(
            &mut state,
            &mut internal.shared_trade,
            snapshot,
            TargetExtensionRequest {
                plan_supplied_target: extension_target,
                position_management,
                minimum_rr: parameters.minimum_rr,
            },
        )? {
            state.internal = serde_json::to_value(&internal)
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: vec![directive],
                rejected_candidates: Vec::new(),
            });
        }
        state.internal = serde_json::to_value(&internal)
            .map_err(|error| PlanError::Serialization(error.to_string()))?;
        return Ok(empty_evaluation(state));
    }
    let signal = state
        .active_signal
        .as_ref()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("mean directive time overflow".to_owned()))?;
    let kind = if signal.execution_plan.is_some() {
        DirectiveKind::EmergencyExit
    } else {
        DirectiveKind::SuggestClose
    };
    if kind == DirectiveKind::EmergencyExit {
        state.trade_health = execution_domain::TradeHealth::Invalid;
    }
    let directive = ManagementDirective {
        directive_id: deterministic_uuid(&[
            signal.signal_id.as_bytes(),
            kind.as_str().as_bytes(),
            b"MEAN_REGIME_INVALIDATED",
            &evaluated.to_be_bytes(),
        ]),
        signal_id: signal.signal_id,
        virtual_trade_id: state.active_virtual_trade_id,
        kind,
        effective_at,
        price: None,
        reason: "MEAN_REGIME_INVALIDATED".to_owned(),
        source_closed_candle_time: evaluated,
        algorithm_version: signal.algorithm_version.clone(),
        parameter_version: signal.parameter_version.clone(),
        reverse_signal: None,
    };
    Ok(PlanEvaluation {
        next_state: state,
        signal: None,
        events: Vec::new(),
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    })
}
