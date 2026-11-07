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
    ConfirmedSwing, LiquidityPool, LiquiditySide, StructureEvent, StructureEventKind, SwingKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    FactLifecycle, HtfAlignment, ManagementDirective, PlanFeedback, PlanKind, PriceZone,
    QuasimodoParameters, ReferenceLevel, SetupState, SignalDefinition, SignalEvent,
    SignalEventKind, TradeState,
};
use rust_decimal::Decimal;

pub struct QuasimodoEvaluator;

/// Minimum |head − shoulder| separation as a fraction of ATR so the pattern is
/// not a trivial two-swing noise structure on LTF gold.
fn min_head_shoulder_atr_fraction() -> Decimal {
    Decimal::new(5, 1) // 0.5
}

struct QuasimodoSetup {
    direction: Direction,
    shoulder: ConfirmedSwing,
    head: ConfirmedSwing,
    neckline: ConfirmedSwing,
    structure_break: StructureEvent,
    target_id: FactId,
    target_level: Decimal,
    atr_id: FactId,
    atr: Decimal,
}

impl PlanEvaluator for QuasimodoEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Quasimodo
    }

    fn algorithm_version(&self) -> &'static str {
        "qm-v4"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::Quasimodo
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::Quasimodo(parameters) = &profile.parameters else {
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

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &QuasimodoParameters,
) -> Result<Option<QuasimodoSetup>, PlanError> {
    let mut highs = typed_facts::<ConfirmedSwing>(&snapshot.target.facts, FactKind::SwingHigh)?;
    let mut lows = typed_facts::<ConfirmedSwing>(&snapshot.target.facts, FactKind::SwingLow)?;
    highs.sort_by_key(|swing| swing.confirmed_at);
    lows.sort_by_key(|swing| swing.confirmed_at);
    let mut structures =
        typed_facts::<StructureEvent>(&snapshot.target.facts, FactKind::Structure)?;
    structures.sort_by_key(|event| event.id.confirmed_at);
    for structure_break in structures
        .into_iter()
        .rev()
        .filter(|event| event.kind == StructureEventKind::BreakOfStructure)
    {
        let (directional_swings, neckline_swings, swing_kind) =
            if structure_break.direction == Direction::Sell {
                (&highs, &lows, SwingKind::High)
            } else {
                (&lows, &highs, SwingKind::Low)
            };
        let matching: Vec<_> = directional_swings
            .iter()
            .filter(|swing| swing.confirmed_at < structure_break.id.confirmed_at)
            .collect();
        let Some(pair) = matching.windows(2).last() else {
            continue;
        };
        let shoulder = pair[0];
        let head = pair[1];
        let valid_extreme = if structure_break.direction == Direction::Sell {
            head.price > shoulder.price
        } else {
            head.price < shoulder.price
        };
        if head.kind != swing_kind || !valid_extreme {
            continue;
        }
        let Some(neckline) = neckline_swings
            .iter()
            .find(|swing| swing.id == structure_break.broken_swing_id)
        else {
            continue;
        };
        if neckline.confirmed_at <= shoulder.confirmed_at
            || neckline.confirmed_at >= structure_break.id.confirmed_at
        {
            continue;
        }
        let structure_age = snapshot
            .evaluated_closed_candle_time
            .saturating_sub(structure_break.id.confirmed_at);
        let max_structure_age = snapshot
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_structure_age_bars))
            .ok_or_else(|| PlanError::Serialization("QM structure age overflow".to_owned()))?;
        if structure_age > max_structure_age {
            continue;
        }
        let Some(atr) = decimal_fact(snapshot, FactKind::Atr)? else {
            continue;
        };
        if atr <= Decimal::ZERO {
            continue;
        }
        // Require a meaningful head vs shoulder range; LTF noise swings often
        // form trivial two-pivot "patterns" with tiny separation.
        let head_shoulder_range = (head.price - shoulder.price).abs();
        if head_shoulder_range < atr * min_head_shoulder_atr_fraction() {
            continue;
        }
        // Counter-HTF QM entries on gold LTF are a major expectancy drain; only
        // allow aligned / neutral / off / missing HTF context.
        let htf = htf_context(snapshot, structure_break.direction, None);
        if matches!(htf.alignment, HtfAlignment::Counter) {
            continue;
        }
        let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
            continue;
        };
        if (latest_close - shoulder.price).abs() / atr > parameters.max_entry_distance_atr {
            continue;
        }
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
        let stop = if structure_break.direction == Direction::Sell {
            head.price + atr * parameters.atr_buffer_multiplier
        } else {
            head.price - atr * parameters.atr_buffer_multiplier
        };
        let risk = (shoulder.price - stop).abs();
        if risk <= Decimal::ZERO {
            continue;
        }
        let Some((target_id, target_level)) = select_target(
            snapshot,
            structure_break.direction,
            shoulder.price,
            risk,
            parameters,
        )?
        else {
            continue;
        };
        return Ok(Some(QuasimodoSetup {
            direction: structure_break.direction,
            shoulder: shoulder.clone(),
            head: head.clone(),
            neckline: neckline.clone(),
            structure_break,
            target_id,
            target_level,
            atr_id,
            atr,
        }));
    }
    Ok(None)
}

fn select_target(
    snapshot: &CanonicalMarketSnapshot,
    direction: Direction,
    entry: Decimal,
    risk: Decimal,
    parameters: &QuasimodoParameters,
) -> Result<Option<(FactId, Decimal)>, PlanError> {
    let target_swing_kind = if direction == Direction::Sell {
        FactKind::SwingLow
    } else {
        FactKind::SwingHigh
    };
    let mut candidates: Vec<_> =
        typed_facts::<ConfirmedSwing>(&snapshot.target.facts, target_swing_kind)?
            .into_iter()
            .map(|swing| (swing.id, swing.price))
            .collect();
    let target_side = if direction == Direction::Sell {
        LiquiditySide::SellSide
    } else {
        LiquiditySide::BuySide
    };
    candidates.extend(
        typed_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)?
            .into_iter()
            .filter(|pool| pool.side == target_side && pool.lifecycle == FactLifecycle::Confirmed)
            .map(|pool| (pool.id, pool.level)),
    );
    candidates
        .into_iter()
        .filter(|(_, level)| {
            let correct_side = if direction == Direction::Sell {
                *level < entry
            } else {
                *level > entry
            };
            correct_side && (*level - entry).abs() / risk >= parameters.minimum_rr
        })
        .min_by_key(|(_, level)| (*level - entry).abs())
        .map_or(Ok(None), |candidate| Ok(Some(candidate)))
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &QuasimodoParameters,
    setup: &QuasimodoSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let minimum_width = snapshot.price_tick * Decimal::from(parameters.minimum_zone_ticks);
    let atr_width = setup.atr * parameters.tolerance_atr_multiplier;
    let width = minimum_width.max(atr_width);
    let half_width = width / Decimal::TWO;
    let entry_zone = PriceZone::new(
        setup.shoulder.price - half_width,
        setup.shoulder.price + half_width,
    )
    .map_err(|error| PlanError::Serialization(error.to_string()))?;
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        setup.shoulder.price,
        setup.head.price,
        setup.target_level,
        setup.atr,
        parameters.atr_buffer_multiplier,
        entry_zone.high - entry_zone.low,
        snapshot.price_tick,
        &profile.position_management,
    )?
    else {
        return Ok(None);
    };
    let stop_loss = execution_plan.hard_stop;
    let risk = (setup.shoulder.price - stop_loss).abs();
    let reward = (setup.target_level - setup.shoulder.price).abs();
    if risk <= Decimal::ZERO || reward / risk < parameters.minimum_rr {
        return Ok(None);
    }
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("QM effective time overflow".to_owned()))?;
    let expires_at = effective_at.checked_add(
        profile
            .key
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_bars_valid))
            .ok_or_else(|| PlanError::Serialization("QM expiry overflow".to_owned()))?,
    );
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"QUASIMODO",
        &evaluated.to_be_bytes(),
    ]);
    let target_swing_kind = if setup.direction == Direction::Buy {
        FactKind::SwingHigh
    } else {
        FactKind::SwingLow
    };
    let mut extension_candidates: Vec<_> =
        typed_facts::<ConfirmedSwing>(&snapshot.target.facts, target_swing_kind)?
            .into_iter()
            .map(|swing| swing.price)
            .collect();
    let target_side = if setup.direction == Direction::Buy {
        LiquiditySide::BuySide
    } else {
        LiquiditySide::SellSide
    };
    extension_candidates.extend(
        typed_facts::<LiquidityPool>(&snapshot.target.facts, FactKind::Liquidity)?
            .into_iter()
            .filter(|pool| pool.side == target_side && pool.lifecycle == FactLifecycle::Confirmed)
            .map(|pool| pool.level),
    );
    let extension_target = extension_candidates
        .into_iter()
        .filter(|price| match setup.direction {
            Direction::Buy => *price > setup.target_level,
            Direction::Sell => *price < setup.target_level,
            Direction::None => false,
        })
        .min_by_key(|price| (*price - setup.target_level).abs());
    let mut reference_levels = vec![
        ReferenceLevel {
            kind: "QM_LEVEL".to_owned(),
            price: setup.shoulder.price,
        },
        ReferenceLevel {
            kind: "HEAD".to_owned(),
            price: setup.head.price,
        },
    ];
    if let Some(price) = extension_target {
        reference_levels.push(ReferenceLevel {
            kind: "TP2_EXTENSION_TARGET".to_owned(),
            price,
        });
    }
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::Quasimodo,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction: setup.direction,
        execution_instruction: ExecutionInstruction::LimitZone,
        entry_zone: Some(entry_zone),
        projected_entry: None,
        stop_loss,
        primary_target: setup.target_level,
        execution_plan: Some(execution_plan),
        reference_levels,
        risk_reward: (reward / risk).round_dp(6),
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: vec![
            evidence(&setup.shoulder.id, "LEFT_SHOULDER"),
            evidence(&setup.head.id, "HEAD"),
            evidence(&setup.neckline.id, "STRUCTURE_BREAK_SWING"),
            evidence(&setup.structure_break.id, "STRUCTURE_BREAK"),
            evidence(&setup.target_id, "TARGET"),
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
    parameters: &analysis_domain::QuasimodoParameters,
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
    let target_swept = signal
        .evidence
        .iter()
        .find(|item| item.reason == "TARGET")
        .and_then(|item| {
            snapshot
                .target
                .facts
                .iter()
                .find(|fact| fact.id == item.fact_id)
        })
        .is_some_and(|fact| fact.lifecycle == FactLifecycle::Swept);
    let opposite_structure = latest_structure
        .as_ref()
        .is_some_and(|event| event.direction != signal.direction);
    let (kind, event_kind, reason) = if state.trade_state == TradeState::NotEntered
        && signal.expires_at.is_some_and(|expiry| evaluated >= expiry)
    {
        (
            DirectiveKind::ExpireEntry,
            Some(SignalEventKind::Expired),
            "QM_ENTRY_EXPIRED",
        )
    } else if state.trade_state == TradeState::NotEntered && (opposite_structure || target_swept) {
        (
            DirectiveKind::CancelEntry,
            Some(SignalEventKind::Invalidated),
            if target_swept {
                "QM_TARGET_SWEPT"
            } else {
                "QM_HEAD_OR_STRUCTURE_INVALIDATED"
            },
        )
    } else if state.trade_state == TradeState::Open && opposite_structure {
        (
            DirectiveKind::EmergencyExit,
            None,
            "QM_OPPOSITE_STRUCTURE_BREAK",
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
        .ok_or_else(|| PlanError::Serialization("QM directive time overflow".to_owned()))?;
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
