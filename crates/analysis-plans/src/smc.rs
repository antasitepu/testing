use crate::common::{
    TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact, deterministic_uuid,
    empty_evaluation, evidence, htf_context, immutable_extension_target,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
    shared_trade_state_from_value, store_or_clear_shared_trade_state, store_shared_trade_state,
    typed_facts,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::{
    ConfirmedSwing, Displacement, FairValueGap, FairValueGapDirection, LiquidityPool,
    LiquiditySide, StructureEvent, StructureEventKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactKind,
    FactLifecycle, HtfFilterMode, ManagementDirective, PlanFeedback, PlanKind, ReferenceLevel,
    SetupState, SignalDefinition, SignalEvent, SignalEventKind, SmcParameters, TradeState,
};
use rust_decimal::Decimal;

pub struct SmcEvaluator;

struct SmcSetup {
    direction: Direction,
    choch: StructureEvent,
    bos: StructureEvent,
    displacement: Displacement,
    fvg: FairValueGap,
    invalidation_swing: ConfirmedSwing,
    target: LiquidityPool,
    atr: Decimal,
    confidence: Decimal,
}

impl PlanEvaluator for SmcEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Smc
    }

    fn algorithm_version(&self) -> &'static str {
        "smc-v4"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::Smc {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::Smc(parameters) = &profile.parameters else {
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
        if let CandidateDecision::Rejected(rejected) = decision {
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

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SmcParameters,
) -> Result<Option<SmcSetup>, PlanError> {
    let structures = typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?;
    let displacements =
        typed_facts::<Displacement>(&snapshot.target.facts, FactKind::Displacement)?;
    let fvgs = typed_facts::<FairValueGap>(&snapshot.target.facts, FactKind::FairValueGap)?;
    let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
        return Ok(None);
    };
    let max_setup_age = snapshot
        .target_timeframe
        .duration_ms()
        .checked_mul(i64::from(parameters.max_bars_valid))
        .ok_or_else(|| PlanError::Serialization("SMC setup age overflow".to_owned()))?;
    let mut choches: Vec<_> = structures
        .iter()
        .filter(|event| event.kind == StructureEventKind::ChangeOfCharacter)
        .collect();
    choches.sort_by_key(|event| event.id.confirmed_at);
    // Newest first: the snapshot spans about 1000 bars, and its oldest complete setup
    // is long past tradable.
    for choch in choches.into_iter().rev() {
        let direction = choch.direction;
        let Some(displacement) = displacements
            .iter()
            .filter(|fact| {
                fact.direction == direction && fact.id.confirmed_at >= choch.id.confirmed_at
            })
            .min_by_key(|fact| fact.id.confirmed_at)
        else {
            continue;
        };
        let Some(bos) = structures
            .iter()
            .filter(|event| {
                event.kind == StructureEventKind::BreakOfStructure
                    && event.direction == direction
                    && event.id.confirmed_at >= displacement.id.confirmed_at
            })
            .min_by_key(|event| event.id.confirmed_at)
        else {
            continue;
        };
        let setup_age = snapshot
            .evaluated_closed_candle_time
            .saturating_sub(bos.id.confirmed_at);
        if setup_age > max_setup_age {
            continue;
        }
        let fvg_direction = if direction == Direction::Buy {
            FairValueGapDirection::Bullish
        } else {
            FairValueGapDirection::Bearish
        };
        let Some(fvg) = fvgs.iter().find(|fact| {
            fact.direction == fvg_direction
                && fact.related_displacement_id.as_ref() == Some(&displacement.id)
                && fact.lifecycle != FactLifecycle::Filled
        }) else {
            continue;
        };
        let Some(invalidation_swing) =
            latest_invalidation_swing(snapshot, direction, bos.id.confirmed_at)?
        else {
            continue;
        };
        let alignment = htf_alignment(snapshot, parameters, direction)?;
        let Some(confidence) = alignment else {
            continue;
        };
        let entry = (fvg.low + fvg.high) / Decimal::TWO;
        let target_side = if direction == Direction::Buy {
            LiquiditySide::BuySide
        } else {
            LiquiditySide::SellSide
        };
        let Some(target) =
            typed_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)?
                .into_iter()
                .filter(|pool| {
                    pool.side == target_side
                        && pool.lifecycle == FactLifecycle::Confirmed
                        && if direction == Direction::Buy {
                            pool.level > entry
                        } else {
                            pool.level < entry
                        }
                })
                .min_by_key(|pool| (pool.level - entry).abs())
        else {
            continue;
        };
        if !price_awaits_entry(direction, fvg, target.level, latest_close) {
            continue;
        }
        let Some(atr) = decimal_fact(snapshot, FactKind::Atr)? else {
            continue;
        };
        return Ok(Some(SmcSetup {
            direction,
            choch: choch.clone(),
            bos: bos.clone(),
            displacement: displacement.clone(),
            fvg: fvg.clone(),
            invalidation_swing,
            target,
            atr,
            confidence,
        }));
    }
    Ok(None)
}

/// A limit entry needs price on the target side of the gap and still short of the target.
fn price_awaits_entry(
    direction: Direction,
    fvg: &FairValueGap,
    target: Decimal,
    close: Decimal,
) -> bool {
    match direction {
        Direction::Buy => close >= fvg.low && close < target,
        Direction::Sell => close <= fvg.high && close > target,
        Direction::None => false,
    }
}

fn latest_invalidation_swing(
    snapshot: &CanonicalMarketSnapshot,
    direction: Direction,
    confirmed_at: i64,
) -> Result<Option<ConfirmedSwing>, PlanError> {
    let swing_kind = if direction == Direction::Buy {
        FactKind::SwingLow
    } else {
        FactKind::SwingHigh
    };
    Ok(
        typed_facts::<ConfirmedSwing>(&snapshot.target.facts, swing_kind)?
            .into_iter()
            .filter(|swing| swing.confirmed_at <= confirmed_at)
            .max_by_key(|swing| swing.confirmed_at),
    )
}

fn htf_alignment(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SmcParameters,
    direction: Direction,
) -> Result<Option<Decimal>, PlanError> {
    if parameters.htf_mode == HtfFilterMode::Off {
        return Ok(Some(Decimal::ONE));
    }
    let Some(timeframe) = parameters.higher_timeframe else {
        return Ok(None);
    };
    let Some(facts) = snapshot
        .higher_timeframes
        .iter()
        .find(|facts| facts.timeframe == timeframe)
    else {
        return Ok(None);
    };
    let Some(htf) = typed_facts::<StructureEvent>(&facts.facts, FactKind::Structure)?
        .into_iter()
        .max_by_key(|event| event.id.confirmed_at)
    else {
        return Ok(None);
    };
    if htf.direction == direction {
        return Ok(Some(Decimal::ONE));
    }
    match parameters.htf_mode {
        HtfFilterMode::Strict => Ok(None),
        HtfFilterMode::Weighted => Ok(Some(
            (Decimal::ONE - parameters.weighted_countertrend_penalty)
                .max(Decimal::ZERO)
                .round_dp(6),
        )),
        HtfFilterMode::Off => Ok(Some(Decimal::ONE)),
    }
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &SmcParameters,
    setup: &SmcSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let entry = (setup.fvg.low + setup.fvg.high) / Decimal::TWO;
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        entry,
        setup.invalidation_swing.price,
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
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"SMC",
        &evaluated.to_be_bytes(),
    ]);
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("SMC effective time overflow".to_owned()))?;
    let expiry = effective_at.checked_add(
        profile
            .key
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_bars_valid))
            .ok_or_else(|| PlanError::Serialization("SMC expiry overflow".to_owned()))?,
    );
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
        plan: PlanKind::Smc,
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
        confidence: setup.confidence,
        evidence: vec![
            evidence(&setup.choch.id, "CHOCH"),
            evidence(&setup.displacement.id, "DISPLACEMENT"),
            evidence(&setup.bos.id, "BOS"),
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
        expires_at: expiry,
    }))
}

fn manage_active(
    mut state: PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &analysis_domain::SmcParameters,
    mut shared: crate::common::SharedTradeState,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    let latest_structure =
        typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?
            .into_iter()
            .max_by_key(|event| event.id.confirmed_at);
    let evidence_lifecycle = |reason_name: &str, lifecycle: FactLifecycle| {
        signal
            .evidence
            .iter()
            .find(|item| item.reason == reason_name)
            .and_then(|item| {
                snapshot
                    .target
                    .facts
                    .iter()
                    .find(|fact| fact.id == item.fact_id)
            })
            .is_some_and(|fact| fact.lifecycle == lifecycle)
    };
    let (kind, event_kind, reason) = if state.trade_state == TradeState::NotEntered
        && signal.expires_at.is_some_and(|expiry| evaluated >= expiry)
    {
        (
            DirectiveKind::ExpireEntry,
            Some(SignalEventKind::Expired),
            "SMC_ENTRY_EXPIRED",
        )
    } else if state.trade_state == TradeState::NotEntered
        && (latest_structure
            .as_ref()
            .is_some_and(|event| event.direction != signal.direction)
            || evidence_lifecycle("RELATED_FVG", FactLifecycle::Filled)
            || evidence_lifecycle("TARGET_LIQUIDITY", FactLifecycle::Swept))
    {
        (
            DirectiveKind::CancelEntry,
            Some(SignalEventKind::Invalidated),
            "SMC_STRUCTURE_INVALIDATED",
        )
    } else if state.trade_state == TradeState::Open
        && latest_structure
            .as_ref()
            .is_some_and(|event| event.direction != signal.direction)
    {
        (
            DirectiveKind::EmergencyExit,
            None,
            "SMC_POST_ENTRY_REVERSAL",
        )
    } else {
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
    if kind == DirectiveKind::EmergencyExit {
        state.trade_health = execution_domain::TradeHealth::Invalid;
    }
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("SMC directive time overflow".to_owned()))?;
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
    let mut events = Vec::new();
    if let Some(event_kind) = event_kind {
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
