use crate::common::{
    TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact, empty_evaluation, evidence,
    htf_context, immutable_extension_target, materialize_execution_plan,
    shared_profit_protection_directive, shared_target_extension_directive,
    shared_thesis_invalidation_directive, shared_trade_state_from_value,
    store_or_clear_shared_trade_state, store_shared_trade_state, typed_facts,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::{
    ConfirmedSwing, Displacement, FairValueGap, FairValueGapDirection, LiquidityPool,
    LiquiditySide, LiquiditySweep, StructureEvent,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactKind,
    FactLifecycle, IctFvgSelection, IctParameters, IctSweepBiasMode, ManagementDirective,
    PlanFeedback, PlanKind, ReferenceLevel, SetupState, SignalDefinition, SignalEvent,
    SignalEventKind, TradeState,
};
use rust_decimal::Decimal;

pub struct IctEvaluator;

#[derive(Clone)]
struct IctSetup {
    direction: Direction,
    sweep: LiquiditySweep,
    displacement: Displacement,
    structure: StructureEvent,
    fvg: FairValueGap,
    invalidation_swing: ConfirmedSwing,
    target: LiquidityPool,
    atr: Decimal,
}

impl PlanEvaluator for IctEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Ict
    }

    fn algorithm_version(&self) -> &'static str {
        "ict-v3"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::Ict
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::Ict(parameters) = &profile.parameters else {
            return Err(PlanError::SignalIdentityMismatch);
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
                next_state,
                snapshot,
                parameters,
                shared,
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
        let decision = create_signal(&mut next_state, signal.clone())?;
        let mut rejected_candidates = Vec::new();
        if let CandidateDecision::Rejected(rejected) = decision {
            rejected_candidates.push(rejected);
            return Ok(PlanEvaluation {
                next_state,
                signal: None,
                events: Vec::new(),
                directives: Vec::new(),
                rejected_candidates,
            });
        }
        let event = SignalEvent {
            event_id: crate::common::deterministic_uuid(&[
                signal.signal_id.as_bytes(),
                b"ENTRY_AVAILABLE",
            ]),
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
            rejected_candidates,
        })
    }
}

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &IctParameters,
) -> Result<Option<IctSetup>, PlanError> {
    let Some(direction) = bias_direction(snapshot, parameters)? else {
        return Ok(None);
    };
    let Some(sweep) =
        typed_facts::<LiquiditySweep>(&snapshot.target.facts, FactKind::LiquiditySweep)?
            .into_iter()
            .filter(|fact| {
                sweep_matches_bias(fact.direction, direction, parameters.sweep_bias_mode)
            })
            .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let Some(displacement) =
        typed_facts::<Displacement>(&snapshot.target.facts, FactKind::Displacement)?
            .into_iter()
            .filter(|fact| {
                fact.direction == direction && fact.id.confirmed_at >= sweep.id.confirmed_at
            })
            .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let Some(structure) =
        typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?
            .into_iter()
            .filter(|fact| {
                fact.direction == direction && fact.id.confirmed_at >= displacement.id.confirmed_at
            })
            .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let expected_fvg_direction = match direction {
        Direction::Buy => FairValueGapDirection::Bullish,
        Direction::Sell => FairValueGapDirection::Bearish,
        Direction::None => return Ok(None),
    };
    let Some(fvg) = typed_facts::<FairValueGap>(&snapshot.target.facts, FactKind::FairValueGap)?
        .into_iter()
        .filter(|fact| {
            fact.direction == expected_fvg_direction
                && fact.lifecycle != FactLifecycle::Filled
                && match parameters.fvg_selection {
                    IctFvgSelection::RelatedDisplacement => {
                        fact.related_displacement_id.as_ref() == Some(&displacement.id)
                    }
                    IctFvgSelection::NearestAfterDisplacement => {
                        fact.id.confirmed_at >= displacement.id.confirmed_at
                    }
                }
        })
        .min_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let Some(invalidation_swing) =
        latest_invalidation_swing(snapshot, direction, structure.id.confirmed_at)?
    else {
        return Ok(None);
    };
    let target_side = match direction {
        Direction::Buy => LiquiditySide::BuySide,
        Direction::Sell => LiquiditySide::SellSide,
        Direction::None => return Ok(None),
    };
    let entry = (fvg.low + fvg.high) / Decimal::TWO;
    let Some(target) = typed_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)?
        .into_iter()
        .filter(|fact| {
            fact.side == target_side
                && fact.lifecycle == FactLifecycle::Confirmed
                && match direction {
                    Direction::Buy => fact.level > entry,
                    Direction::Sell => fact.level < entry,
                    Direction::None => false,
                }
        })
        .min_by_key(|fact| (fact.level - entry).abs())
    else {
        return Ok(None);
    };
    let atr = decimal_fact(snapshot, FactKind::Atr)?.unwrap_or(Decimal::ZERO);
    if atr <= Decimal::ZERO {
        return Ok(None);
    }
    Ok(Some(IctSetup {
        direction,
        sweep,
        displacement,
        structure,
        fvg,
        invalidation_swing,
        target,
        atr,
    }))
}

fn bias_direction(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &IctParameters,
) -> Result<Option<Direction>, PlanError> {
    if let Some(higher_timeframe) = parameters.higher_timeframe {
        let Some(higher) = snapshot
            .higher_timeframes
            .iter()
            .find(|facts| facts.timeframe == higher_timeframe)
        else {
            return Ok(None);
        };
        return Ok(
            typed_facts::<StructureEvent>(&higher.facts, FactKind::Structure)?
                .into_iter()
                .max_by_key(|fact| fact.id.confirmed_at)
                .map(|structure| structure.direction),
        );
    }
    Ok(
        typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?
            .into_iter()
            .max_by_key(|fact| fact.id.confirmed_at)
            .map(|structure| structure.direction),
    )
}

fn sweep_matches_bias(
    sweep_direction: Direction,
    bias_direction: Direction,
    mode: IctSweepBiasMode,
) -> bool {
    match mode {
        IctSweepBiasMode::SameBias => sweep_direction == bias_direction,
        IctSweepBiasMode::OppositeBias => {
            sweep_direction != bias_direction && sweep_direction != Direction::None
        }
    }
}

fn latest_invalidation_swing(
    snapshot: &CanonicalMarketSnapshot,
    direction: Direction,
    confirmed_at: i64,
) -> Result<Option<ConfirmedSwing>, PlanError> {
    let swing_kind = match direction {
        Direction::Buy => FactKind::SwingLow,
        Direction::Sell => FactKind::SwingHigh,
        Direction::None => return Ok(None),
    };
    Ok(
        typed_facts::<ConfirmedSwing>(&snapshot.target.facts, swing_kind)?
            .into_iter()
            .filter(|swing| swing.confirmed_at <= confirmed_at)
            .max_by_key(|swing| swing.confirmed_at),
    )
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &IctParameters,
    setup: &IctSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let entry = (setup.fvg.low + setup.fvg.high) / Decimal::TWO;
    let thesis = match setup.direction {
        Direction::Buy => setup.sweep.extreme.min(setup.invalidation_swing.price),
        Direction::Sell => setup.sweep.extreme.max(setup.invalidation_swing.price),
        Direction::None => return Ok(None),
    };
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        entry,
        thesis,
        setup.target.level,
        setup.atr,
        parameters.atr_buffer_multiplier,
        setup.fvg.high - setup.fvg.low,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let stop_loss = execution_plan.hard_stop;
    let risk = (entry - stop_loss).abs();
    let reward = (setup.target.level - entry).abs();
    if risk <= Decimal::ZERO || reward / risk < parameters.minimum_rr {
        return Ok(None);
    }
    let evaluated = snapshot.evaluated_closed_candle_time;
    let evaluated_bytes = evaluated.to_be_bytes();
    let signal_id = crate::common::deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"ICT",
        &evaluated_bytes,
    ]);
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("ICT effective time overflow".to_owned()))?;
    let expiry_duration = profile
        .key
        .target_timeframe
        .duration_ms()
        .checked_mul(i64::from(parameters.entry_max_bars_valid))
        .ok_or_else(|| PlanError::Serialization("ICT expiry overflow".to_owned()))?;
    let extension_target =
        typed_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)?
            .into_iter()
            .filter(|pool| {
                pool.side == setup.target.side
                    && pool.lifecycle == FactLifecycle::Confirmed
                    && match setup.direction {
                        Direction::Buy => pool.level > setup.target.level,
                        Direction::Sell => pool.level < setup.target.level,
                        Direction::None => false,
                    }
            })
            .min_by_key(|pool| (pool.level - setup.target.level).abs())
            .map(|pool| pool.level);
    let mut reference_levels = vec![ReferenceLevel {
        kind: "OPPOSING_LIQUIDITY".to_owned(),
        price: setup.target.level,
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
        plan: PlanKind::Ict,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction: setup.direction,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(
            analysis_domain::PriceZone::new(setup.fvg.low, setup.fvg.high)
                .map_err(|error| PlanError::Serialization(error.to_string()))?,
        ),
        projected_entry: None,
        stop_loss,
        primary_target: setup.target.level,
        execution_plan: Some(execution_plan),
        reference_levels,
        risk_reward: (reward / risk).round_dp(6),
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: vec![
            evidence(&setup.sweep.id, "LIQUIDITY_SWEEP"),
            evidence(&setup.displacement.id, "DISPLACEMENT"),
            evidence(&setup.structure.id, "STRUCTURE_BREAK"),
            evidence(&setup.fvg.id, "RELATED_FVG"),
            evidence(&setup.invalidation_swing.id, "INVALIDATION_SWING"),
            evidence(&setup.target.id, "TARGET_LIQUIDITY"),
        ],
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
        expires_at: effective_at.checked_add(expiry_duration),
    }))
}

fn manage_active(
    mut state: PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &analysis_domain::IctParameters,
    mut shared: crate::common::SharedTradeState,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    let mut event_kind = None;
    let mut directive_kind = None;
    let mut reason = None;
    if state.trade_state == TradeState::NotEntered
        && signal
            .expires_at
            .is_some_and(|expires_at| evaluated >= expires_at)
    {
        event_kind = Some(SignalEventKind::Expired);
        directive_kind = Some(DirectiveKind::ExpireEntry);
        reason = Some("ICT_ENTRY_EXPIRED");
    } else {
        let latest_target_structure =
            typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?
                .into_iter()
                .max_by_key(|fact| fact.id.confirmed_at);
        let latest_htf_structure = snapshot
            .higher_timeframes
            .iter()
            .flat_map(|facts| {
                typed_facts::<StructureEvent>(&facts.facts, FactKind::Structure).unwrap_or_default()
            })
            .max_by_key(|fact| fact.id.confirmed_at);
        let evidence_lifecycle = |reason_name: &str, lifecycle: FactLifecycle| {
            signal
                .evidence
                .iter()
                .find(|evidence| evidence.reason == reason_name)
                .and_then(|evidence| {
                    snapshot
                        .target
                        .facts
                        .iter()
                        .find(|fact| fact.id == evidence.fact_id)
                })
                .is_some_and(|fact| fact.lifecycle == lifecycle)
        };
        if state.trade_state == TradeState::NotEntered
            && (latest_htf_structure
                .as_ref()
                .is_some_and(|structure| structure.direction != signal.direction)
                || evidence_lifecycle("RELATED_FVG", FactLifecycle::Filled)
                || evidence_lifecycle("TARGET_LIQUIDITY", FactLifecycle::Swept)
                || latest_target_structure
                    .as_ref()
                    .is_some_and(|structure| structure.direction != signal.direction))
        {
            event_kind = Some(SignalEventKind::Invalidated);
            directive_kind = Some(DirectiveKind::CancelEntry);
            reason = Some("ICT_PRE_ENTRY_INVALIDATED");
        } else if state.trade_state == TradeState::Open
            && latest_target_structure
                .as_ref()
                .is_some_and(|structure| structure.direction != signal.direction)
        {
            directive_kind = Some(DirectiveKind::EmergencyExit);
            reason = Some("ICT_POST_ENTRY_STRUCTURE_REVERSAL");
        }
    }
    let Some(directive_kind) = directive_kind else {
        if let Some(directive) = shared_thesis_invalidation_directive(&mut state, snapshot)? {
            store_shared_trade_state(&mut state, &shared)?;
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
            &mut shared,
            snapshot,
            profit_protection,
            position_management,
        )? {
            store_shared_trade_state(&mut state, &shared)?;
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: vec![directive],
                rejected_candidates: Vec::new(),
            });
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
            return Ok(PlanEvaluation {
                next_state: state,
                signal: None,
                events: Vec::new(),
                directives: vec![directive],
                rejected_candidates: Vec::new(),
            });
        }
        store_shared_trade_state(&mut state, &shared)?;
        return Ok(empty_evaluation(state));
    };
    if directive_kind == DirectiveKind::EmergencyExit {
        state.trade_health = execution_domain::TradeHealth::Invalid;
    }
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("ICT directive time overflow".to_owned()))?;
    let reason = reason.expect("directive reason is set");
    let directive = ManagementDirective {
        directive_id: crate::common::deterministic_uuid(&[
            signal.signal_id.as_bytes(),
            directive_kind.as_str().as_bytes(),
            reason.as_bytes(),
            &evaluated.to_be_bytes(),
        ]),
        signal_id: signal.signal_id,
        virtual_trade_id: state.active_virtual_trade_id,
        kind: directive_kind,
        effective_at,
        price: None,
        reason: reason.to_owned(),
        source_closed_candle_time: evaluated,
        algorithm_version: signal.algorithm_version.clone(),
        parameter_version: signal.parameter_version.clone(),
        reverse_signal: None,
    };
    let mut events = Vec::new();
    if let Some(kind) = event_kind {
        let event = SignalEvent {
            event_id: crate::common::deterministic_uuid(&[
                signal.signal_id.as_bytes(),
                kind.as_str().as_bytes(),
                &evaluated.to_be_bytes(),
            ]),
            signal_id: signal.signal_id,
            sequence: state.next_event_sequence,
            kind,
            effective_at,
            recorded_at: effective_at,
            virtual_trade_id: state.active_virtual_trade_id,
            price: None,
            reason: Some(reason.to_owned()),
        };
        append_event(&mut state, &event)?;
        events.push(event);
    }
    Ok(PlanEvaluation {
        next_state: state,
        signal: None,
        events,
        directives: vec![directive],
        rejected_candidates: Vec::new(),
    })
}
