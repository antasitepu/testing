use crate::common::{
    SharedTradeState, TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact,
    deterministic_uuid, empty_evaluation, evidence, htf_context, immutable_extension_target,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
    shared_trade_state_from_value, store_shared_trade_state, typed_facts,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::{ChandelierFact, ConfirmedSwing};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    HtfAlignment, ManagementDirective, PlanFeedback, PlanKind, ReferenceLevel, SetupState,
    SignalDefinition, SignalEvent, SignalEventKind, TradeState, TrendBreakoutParameters,
};
use rust_decimal::Decimal;
use serde::Deserialize;

pub struct TrendBreakoutEvaluator;

/// Minimum breakout distance beyond the Donchian band, as a fraction of ATR.
/// Filters single-tick / noise closes that only barely pierce the channel.
fn min_breakout_atr_fraction() -> Decimal {
    Decimal::new(1, 1) // 0.1
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DonchianObservation {
    period: usize,
    upper: Decimal,
    lower: Decimal,
    source_end_time: i64,
    close: Decimal,
}

#[derive(Clone, Debug, Deserialize)]
struct EmaObservation {
    value: Decimal,
    slope: Decimal,
}

struct TrendSetup {
    direction: Direction,
    donchian_id: FactId,
    donchian: DonchianObservation,
    ema_id: FactId,
    adx_id: FactId,
    atr_id: FactId,
    atr: Decimal,
    htf_ema_id: Option<FactId>,
}

impl PlanEvaluator for TrendBreakoutEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::TrendBreakout
    }

    fn algorithm_version(&self) -> &'static str {
        "trend-v4"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::TrendBreakout
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::TrendBreakout(parameters) = &profile.parameters else {
            return Err(PlanError::SignalIdentityMismatch);
        };
        let mut next_state = state.clone();
        let mut internal = shared_trade_state_from_value(&next_state.internal)?;
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
            apply_shared_trade_feedback(&mut internal, &next_state, item)?;
        }
        if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
            next_state.clear_terminal_trade();
            next_state.internal = serde_json::Value::Null;
            return Ok(empty_evaluation(next_state));
        }
        store_shared_trade_state(&mut next_state, &internal)?;
        if snapshot.readiness != analysis_domain::Readiness::Ready {
            next_state.readiness = snapshot.readiness;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_some() {
            return manage_active(
                next_state,
                profile,
                snapshot,
                parameters,
                internal,
                &profile.profit_protection,
                &profile.position_management,
            );
        }
        let Some(setup) = find_setup(snapshot, parameters)? else {
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

fn latest_donchian(
    snapshot: &CanonicalMarketSnapshot,
) -> Result<Option<(FactId, DonchianObservation)>, PlanError> {
    snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Donchian)
        .max_by_key(|fact| fact.id.confirmed_at)
        .map(|fact| {
            let observation = serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok((fact.id.clone(), observation))
        })
        .transpose()
}

fn raw_breakout(
    snapshot: &CanonicalMarketSnapshot,
) -> Result<Option<(Direction, DonchianObservation)>, PlanError> {
    let Some((_, observation)) = latest_donchian(snapshot)? else {
        return Ok(None);
    };
    let direction = if observation.close > observation.upper {
        Some(Direction::Buy)
    } else if observation.close < observation.lower {
        Some(Direction::Sell)
    } else {
        None
    };
    Ok(direction.map(|direction| (direction, observation)))
}

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &TrendBreakoutParameters,
) -> Result<Option<TrendSetup>, PlanError> {
    let Some((donchian_id, donchian)) = latest_donchian(snapshot)? else {
        return Ok(None);
    };
    if donchian.period != usize::try_from(parameters.donchian_period).unwrap_or_default()
        || donchian.source_end_time >= snapshot.evaluated_closed_candle_time
    {
        return Ok(None);
    }
    let direction = if donchian.close > donchian.upper {
        Direction::Buy
    } else if donchian.close < donchian.lower {
        Direction::Sell
    } else {
        return Ok(None);
    };
    let Some(atr) = decimal_fact(snapshot, FactKind::Atr)? else {
        return Ok(None);
    };
    if atr <= Decimal::ZERO {
        return Ok(None);
    }
    // Reject noise pierces: close must clear the band by a meaningful ATR fraction.
    let breakout_margin = (atr * min_breakout_atr_fraction()).max(snapshot.price_tick);
    let breakout_distance = if direction == Direction::Buy {
        donchian.close - donchian.upper
    } else {
        donchian.lower - donchian.close
    };
    if breakout_distance < breakout_margin {
        return Ok(None);
    }
    let Some(ema_fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Ema)
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let ema: EmaObservation = serde_json::from_value(ema_fact.value.clone())
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    let slope_aligned = if direction == Direction::Buy {
        ema.slope > Decimal::ZERO
    } else {
        ema.slope < Decimal::ZERO
    };
    if !slope_aligned || ema.value <= Decimal::ZERO {
        return Ok(None);
    }
    // LTF close should also sit on the trend side of the LTF EMA (not only slope sign).
    let price_vs_ema = if direction == Direction::Buy {
        donchian.close > ema.value
    } else {
        donchian.close < ema.value
    };
    if !price_vs_ema {
        return Ok(None);
    }
    let Some(adx_fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Adx)
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let adx: Decimal = serde_json::from_value(
        adx_fact
            .value
            .get("value")
            .cloned()
            .unwrap_or_else(|| adx_fact.value.clone()),
    )
    .map_err(|error| PlanError::Serialization(error.to_string()))?;
    if adx < parameters.minimum_adx {
        return Ok(None);
    }
    let htf_ema_id = if let Some(timeframe) = parameters.higher_timeframe {
        let Some(facts) = snapshot
            .higher_timeframes
            .iter()
            .find(|facts| facts.timeframe == timeframe)
        else {
            return Ok(None);
        };
        let Some(fact) = facts
            .facts
            .iter()
            .filter(|fact| fact.id.kind == FactKind::Ema)
            .max_by_key(|fact| fact.id.confirmed_at)
        else {
            return Ok(None);
        };
        let htf: EmaObservation = serde_json::from_value(fact.value.clone())
            .map_err(|error| PlanError::Serialization(error.to_string()))?;
        let aligned = if direction == Direction::Buy {
            htf.slope > Decimal::ZERO
        } else {
            htf.slope < Decimal::ZERO
        };
        if !aligned {
            return Ok(None);
        }
        // Prefer HTF structure agreement when present; skip pure counter-structure.
        let htf = htf_context(snapshot, direction, parameters.higher_timeframe);
        if matches!(htf.alignment, HtfAlignment::Counter) {
            return Ok(None);
        }
        Some(fact.id.clone())
    } else {
        None
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
    Ok(Some(TrendSetup {
        direction,
        donchian_id,
        donchian,
        ema_id: ema_fact.id.clone(),
        adx_id: adx_fact.id.clone(),
        atr_id,
        atr,
        htf_ema_id,
    }))
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &TrendBreakoutParameters,
    setup: &TrendSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let entry = setup.donchian.close;
    let initial_risk = setup.atr * parameters.initial_stop_atr_multiplier;
    let atr_stop = if setup.direction == Direction::Buy {
        entry - initial_risk
    } else {
        entry + initial_risk
    };
    let primary_target = if setup.direction == Direction::Buy {
        entry + initial_risk * parameters.target_rr
    } else {
        entry - initial_risk * parameters.target_rr
    };
    if atr_stop <= Decimal::ZERO || primary_target <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "trend risk levels must remain positive".to_owned(),
        ));
    }
    let breakout_level = if setup.direction == Direction::Buy {
        setup.donchian.upper
    } else {
        setup.donchian.lower
    };
    let Some(mut execution_plan) = materialize_execution_plan(
        setup.direction,
        entry,
        breakout_level,
        primary_target,
        setup.atr,
        parameters.initial_stop_atr_multiplier,
        Decimal::ZERO,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    execution_plan.hard_stop = match setup.direction {
        Direction::Buy => execution_plan.hard_stop.min(atr_stop),
        Direction::Sell => execution_plan.hard_stop.max(atr_stop),
        Direction::None => return Ok(None),
    };
    execution_plan
        .validate(setup.direction, entry, primary_target)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    let stop_loss = execution_plan.hard_stop;
    let final_risk = (entry - stop_loss).abs();
    if final_risk <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "trend final risk must remain positive".to_owned(),
        ));
    }
    let risk_reward = ((primary_target - entry).abs() / final_risk).round_dp(6);
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("trend effective time overflow".to_owned()))?;
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"TREND_BREAKOUT",
        &evaluated.to_be_bytes(),
    ]);
    let mut signal_evidence = vec![
        evidence(&setup.donchian_id, "PREVIOUS_DONCHIAN"),
        evidence(&setup.ema_id, "EMA_ALIGNMENT"),
        evidence(&setup.adx_id, "ADX_STRENGTH"),
        evidence(&setup.atr_id, "INITIAL_ATR_STOP"),
    ];
    if let Some(id) = &setup.htf_ema_id {
        signal_evidence.push(evidence(id, "HTF_EMA_ALIGNMENT"));
    }
    let target_swing_kind = if setup.direction == Direction::Buy {
        FactKind::SwingHigh
    } else {
        FactKind::SwingLow
    };
    let extension_target =
        typed_facts::<ConfirmedSwing>(&snapshot.target.facts, target_swing_kind)?
            .into_iter()
            .map(|swing| swing.price)
            .filter(|price| match setup.direction {
                Direction::Buy => *price > primary_target,
                Direction::Sell => *price < primary_target,
                Direction::None => false,
            })
            .min_by_key(|price| (*price - primary_target).abs());
    let mut reference_levels = vec![ReferenceLevel {
        kind: "BREAKOUT_LEVEL".to_owned(),
        price: breakout_level,
    }];
    if let Some(price) = extension_target {
        reference_levels.push(ReferenceLevel {
            kind: "TP2_EXTENSION_TARGET".to_owned(),
            price,
        });
    }
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction: setup.direction,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(entry),
        stop_loss,
        primary_target,
        execution_plan: Some(execution_plan),
        reference_levels,
        risk_reward,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: signal_evidence,
        htf_context: Some(htf_context(
            snapshot,
            setup.direction,
            parameters.higher_timeframe,
        )),
        effective_at,
        detected_at: effective_at,
        published_at: effective_at,
        release_id: profile.versions.release_id,
        algorithm_version: profile.versions.algorithm_version.clone(),
        parameter_version: profile.versions.parameter_version.clone(),
        created_at: effective_at,
        expires_at: None,
    }))
}

fn manage_active(
    mut state: PlanRuntimeState,
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &TrendBreakoutParameters,
    mut internal: SharedTradeState,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    if state.trade_state == TradeState::Closed {
        state.clear_terminal_trade();
        state.internal = serde_json::Value::Null;
        return Ok(empty_evaluation(state));
    }
    let evaluated = snapshot.evaluated_closed_candle_time;

    // NEXT_OPEN recommendations that never fill must not occupy the slot forever,
    // and false breakouts that close back inside the channel should cancel.
    if state.trade_state == TradeState::NotEntered {
        let Some((_, donchian)) = latest_donchian(snapshot)? else {
            return Ok(empty_evaluation(state));
        };
        let still_outside = match signal.direction {
            Direction::Buy => donchian.close > donchian.upper,
            Direction::Sell => donchian.close < donchian.lower,
            Direction::None => false,
        };
        if !still_outside {
            return directive_evaluation(
                state,
                &signal,
                evaluated,
                DirectiveKind::CancelEntry,
                None,
                "TREND_FALSE_BREAKOUT_RECLAIM",
            );
        }
        if evaluated > signal.evaluated_at {
            return directive_evaluation(
                state,
                &signal,
                evaluated,
                DirectiveKind::ExpireEntry,
                None,
                "TREND_NEXT_OPEN_MISSED",
            );
        }
        return Ok(empty_evaluation(state));
    }

    if state.trade_state != TradeState::Open {
        return Ok(empty_evaluation(state));
    }
    if let Some((breakout_direction, _)) = raw_breakout(snapshot)?
        && breakout_direction != signal.direction
    {
        if let Some(setup) = find_setup(snapshot, parameters)?
            && setup.direction == breakout_direction
            && let Some(reverse_signal) = build_signal(profile, snapshot, parameters, &setup)?
        {
            state.trade_health = execution_domain::TradeHealth::Invalid;
            let mut evaluation = directive_evaluation(
                state,
                &signal,
                evaluated,
                DirectiveKind::ReverseAtNextOpen,
                None,
                "TREND_VALID_OPPOSITE_BREAKOUT",
            )?;
            if let Some(directive) = evaluation.directives.first_mut() {
                directive.reverse_signal = Some(reverse_signal);
            }
            return Ok(evaluation);
        }
        state.trade_health = execution_domain::TradeHealth::Invalid;
        return directive_evaluation(
            state,
            &signal,
            evaluated,
            DirectiveKind::EmergencyExit,
            None,
            "TREND_OPPOSITE_BREAKOUT_FILTER_FAILED",
        );
    }
    if let Some(directive) = shared_thesis_invalidation_directive(&mut state, snapshot)? {
        store_shared_trade_state(&mut state, &internal)?;
        return Ok(PlanEvaluation {
            next_state: state,
            signal: None,
            events: Vec::new(),
            directives: vec![directive],
            rejected_candidates: Vec::new(),
        });
    }
    let Some(entry_price) = internal.entry_price else {
        return Ok(empty_evaluation(state));
    };
    let Some(initial_risk) = internal.initial_risk else {
        return Ok(empty_evaluation(state));
    };
    let Some((_, donchian)) = latest_donchian(snapshot)? else {
        return Ok(empty_evaluation(state));
    };
    let profit = if signal.direction == Direction::Buy {
        donchian.close - entry_price
    } else {
        entry_price - donchian.close
    };
    let current_stop = internal.active_stop.unwrap_or(signal.stop_loss);
    let protection = shared_profit_protection_directive(
        &mut state,
        &mut internal,
        snapshot,
        profit_protection,
        position_management,
    )?;
    if protection
        .as_ref()
        .is_some_and(|directive| directive.kind == DirectiveKind::SuggestClose)
    {
        store_shared_trade_state(&mut state, &internal)?;
        return Ok(PlanEvaluation {
            next_state: state,
            signal: None,
            events: Vec::new(),
            directives: protection.into_iter().collect(),
            rejected_candidates: Vec::new(),
        });
    }
    if state.position_stage != execution_domain::PositionStage::Runner {
        store_shared_trade_state(&mut state, &internal)?;
        return Ok(empty_evaluation(state));
    }
    let stepped_candidate = protection.as_ref().and_then(|directive| directive.price);
    let chandelier_candidate =
        if profit >= initial_risk * trend_trailing_activation_r(&signal, parameters) {
            typed_facts::<ChandelierFact>(&snapshot.target.facts, FactKind::Chandelier)?
                .into_iter()
                .max_by_key(|fact| fact.source_end_time)
                .map(|chandelier| match signal.direction {
                    Direction::Buy => chandelier.long_candidate,
                    Direction::Sell => chandelier.short_candidate,
                    Direction::None => current_stop,
                })
                .filter(|candidate| match signal.direction {
                    Direction::Buy => *candidate > current_stop && *candidate < donchian.close,
                    Direction::Sell => *candidate < current_stop && *candidate > donchian.close,
                    Direction::None => false,
                })
        } else {
            None
        };
    let selected_stop = match signal.direction {
        Direction::Buy => stepped_candidate
            .into_iter()
            .chain(chandelier_candidate)
            .max(),
        Direction::Sell => stepped_candidate
            .into_iter()
            .chain(chandelier_candidate)
            .min(),
        Direction::None => None,
    };
    if let Some(candidate) = selected_stop {
        let stepped_wins = stepped_candidate == Some(candidate);
        internal.active_stop = Some(candidate);
        store_shared_trade_state(&mut state, &internal)?;
        if stepped_wins {
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: protection.into_iter().collect(),
                rejected_candidates: Vec::new(),
            });
        }
        return directive_evaluation(
            state,
            &signal,
            evaluated,
            DirectiveKind::MoveStop,
            Some(candidate),
            "TREND_CHANDELIER_TRAIL",
        );
    }
    if let Some(directive) = shared_target_extension_directive(
        &mut state,
        &mut internal,
        snapshot,
        TargetExtensionRequest {
            plan_supplied_target: immutable_extension_target(&signal),
            position_management,
            minimum_rr: parameters.target_rr,
        },
    )? {
        store_shared_trade_state(&mut state, &internal)?;
        return Ok(PlanEvaluation {
            next_state: state,
            signal: None,
            events: Vec::new(),
            directives: vec![directive],
            rejected_candidates: Vec::new(),
        });
    }
    store_shared_trade_state(&mut state, &internal)?;
    Ok(empty_evaluation(state))
}

fn trend_trailing_activation_r(
    signal: &SignalDefinition,
    parameters: &TrendBreakoutParameters,
) -> Decimal {
    match signal.htf_context.as_ref().map(|context| context.alignment) {
        Some(HtfAlignment::Counter) => parameters.trailing_activation_r.min(Decimal::new(5, 1)),
        _ => parameters.trailing_activation_r,
    }
}

fn directive_evaluation(
    state: PlanRuntimeState,
    signal: &SignalDefinition,
    evaluated: i64,
    kind: DirectiveKind,
    price: Option<Decimal>,
    reason: &str,
) -> Result<PlanEvaluation, PlanError> {
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("trend directive time overflow".to_owned()))?;
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
        price,
        reason: reason.to_owned(),
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
