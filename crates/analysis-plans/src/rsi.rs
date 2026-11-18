use crate::common::{
    SharedTradeState, TargetExtensionRequest, apply_shared_trade_feedback, decimal_fact,
    deterministic_uuid, empty_evaluation, evidence, htf_context, immutable_extension_target,
    materialize_execution_plan, shared_profit_protection_directive,
    shared_target_extension_directive, shared_thesis_invalidation_directive,
    shared_trade_state_from_value, store_or_clear_shared_trade_state, store_shared_trade_state,
    typed_facts,
};
use crate::{
    CandidateDecision, PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState, append_event,
    apply_feedback, create_signal,
};
use analysis_core::{ConfirmedSwing, DivergenceKind, RegularDivergence, SwingKind};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, DirectiveKind, ExecutionInstruction, FactId, FactKind,
    FactLifecycle, ManagementDirective, PlanFeedback, PlanKind, ReferenceLevel, RsiPlanParameters,
    SetupState, SignalDefinition, SignalEvent, SignalEventKind, TradeState,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub struct RsiEvaluator;
pub struct RsiV5Evaluator;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RsiInternal {
    armed: Option<RsiArmedSetup>,
    consumed_through: Option<i64>,
    #[serde(default)]
    shared_trade: SharedTradeState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RsiArmedSetup {
    direction: Direction,
    extreme_id: FactId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extreme_identity: Option<StableFactIdentity>,
    extreme_rsi: Decimal,
    extreme_close: Decimal,
    swing_id: FactId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swing_identity: Option<StableFactIdentity>,
    divergence_id: Option<FactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    divergence_identity: Option<StableFactIdentity>,
    armed_at: i64,
    expires_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StableFactIdentity {
    symbol: Symbol,
    timeframe: Timeframe,
    kind: FactKind,
    confirmed_at: i64,
    algorithm_version: String,
    parameter_version: String,
}

impl StableFactIdentity {
    fn from_fact_id(id: &FactId) -> Self {
        Self {
            symbol: id.symbol.clone(),
            timeframe: id.timeframe,
            kind: id.kind,
            confirmed_at: id.confirmed_at,
            algorithm_version: id.algorithm_version.clone(),
            parameter_version: id.parameter_version.clone(),
        }
    }

    fn matches(&self, id: &FactId) -> bool {
        self.symbol == id.symbol
            && self.timeframe == id.timeframe
            && self.kind == id.kind
            && self.confirmed_at == id.confirmed_at
            && self.algorithm_version == id.algorithm_version
            && self.parameter_version == id.parameter_version
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RsiValue {
    value: Decimal,
    close: Decimal,
}

#[derive(Clone)]
struct RsiObservation {
    id: FactId,
    value: Decimal,
    close: Decimal,
}

struct RsiSetup {
    direction: Direction,
    entered: RsiObservation,
    exited: RsiObservation,
    swing: ConfirmedSwing,
    divergence: Option<RegularDivergence>,
    atr_id: FactId,
    atr: Decimal,
}

impl PlanEvaluator for RsiEvaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Rsi
    }

    fn algorithm_version(&self) -> &'static str {
        "rsi-v4"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::Rsi
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::Rsi(parameters) = &profile.parameters else {
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

impl PlanEvaluator for RsiV5Evaluator {
    fn kind(&self) -> PlanKind {
        PlanKind::Rsi
    }

    fn algorithm_version(&self) -> &'static str {
        "rsi-v5"
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        if profile.key.plan != PlanKind::Rsi
            || snapshot.target_timeframe != profile.key.target_timeframe
        {
            return Err(PlanError::SignalIdentityMismatch);
        }
        let analysis_domain::PlanParameters::Rsi(parameters) = &profile.parameters else {
            return Err(PlanError::SignalIdentityMismatch);
        };
        let mut next_state = state.clone();
        let mut internal = load_rsi_internal(&next_state.internal)?;
        for item in feedback {
            apply_feedback(&mut next_state, item)?;
            apply_shared_trade_feedback(&mut internal.shared_trade, &next_state, item)?;
        }
        if next_state.trade_state == TradeState::Closed && next_state.active_signal.is_some() {
            next_state.clear_terminal_trade();
            internal.shared_trade = SharedTradeState::default();
            store_rsi_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        }
        if snapshot.readiness != analysis_domain::Readiness::Ready {
            next_state.readiness = snapshot.readiness;
            if consume_armed(&mut internal) {
                next_state.setup_state = SetupState::Watching;
            }
            store_rsi_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        }
        if next_state.active_signal.is_some() {
            return manage_active_v5(
                next_state,
                snapshot,
                parameters,
                internal,
                &profile.profit_protection,
                &profile.position_management,
            );
        }
        let Some(setup) = evaluate_v5_setup(snapshot, parameters, &mut internal)? else {
            next_state.setup_state = SetupState::Watching;
            store_rsi_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        };
        next_state.setup_state = SetupState::Watching;
        let Some(signal) = build_signal(profile, snapshot, parameters, &setup)? else {
            store_rsi_internal(&mut next_state, &internal)?;
            return Ok(empty_evaluation(next_state));
        };
        if let CandidateDecision::Rejected(rejected) =
            create_signal(&mut next_state, signal.clone())?
        {
            store_rsi_internal(&mut next_state, &internal)?;
            return Ok(PlanEvaluation {
                next_state,
                signal: None,
                events: Vec::new(),
                directives: Vec::new(),
                rejected_candidates: vec![rejected],
            });
        }
        consume_armed_through(&mut internal, Some(setup.exited.id.confirmed_at));
        store_rsi_internal(&mut next_state, &internal)?;
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

fn load_rsi_internal(value: &serde_json::Value) -> Result<RsiInternal, PlanError> {
    if value.is_null() {
        return Ok(RsiInternal::default());
    }
    const TYPED_FIELDS: [&str; 3] = ["armed", "consumedThrough", "sharedTrade"];
    const LEGACY_FIELDS: [&str; 10] = [
        "entryPrice",
        "initialRisk",
        "activeStop",
        "activeTarget",
        "bestClosedProfitR",
        "previousClose",
        "lastEvaluatedCloseTime",
        "adverseCloseCount",
        "profitExitRequestedAt",
        "newMfeOnLastClose",
    ];
    let object = value.as_object().ok_or_else(|| {
        PlanError::Serialization("RSI v5 internal state must be an object or null".to_owned())
    })?;
    let has_typed_field = TYPED_FIELDS.iter().any(|field| object.contains_key(*field));
    let has_legacy_field = LEGACY_FIELDS
        .iter()
        .any(|field| object.contains_key(*field));
    let mut unknown_fields = object
        .keys()
        .filter(|field| {
            !TYPED_FIELDS.contains(&field.as_str()) && !LEGACY_FIELDS.contains(&field.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    unknown_fields.sort();
    if !unknown_fields.is_empty() {
        return Err(PlanError::Serialization(format!(
            "RSI v5 internal state contains unknown field(s): {}",
            unknown_fields.join(", ")
        )));
    }
    if has_typed_field && has_legacy_field {
        return Err(PlanError::Serialization(
            "RSI v5 internal state mixes typed and legacy fields".to_owned(),
        ));
    }
    if has_typed_field {
        let missing_fields = TYPED_FIELDS
            .iter()
            .filter(|field| !object.contains_key(**field))
            .copied()
            .collect::<Vec<_>>();
        if !missing_fields.is_empty() {
            return Err(PlanError::Serialization(format!(
                "RSI v5 typed internal state is missing field(s): {}",
                missing_fields.join(", ")
            )));
        }
        validate_shared_trade_fields(
            object
                .get("sharedTrade")
                .expect("typed field presence was validated"),
            &LEGACY_FIELDS,
        )?;
        validate_persisted_arm_fact_ids(
            object
                .get("armed")
                .expect("typed field presence was validated"),
        )?;
        let mut internal: RsiInternal = serde_json::from_value(value.clone())
            .map_err(|error| PlanError::Serialization(error.to_string()))?;
        if let Some(armed) = internal.armed.as_mut() {
            let armed_object = object
                .get("armed")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    PlanError::Serialization(
                        "RSI persisted armed state must be an object or null".to_owned(),
                    )
                })?;
            validate_required_persisted_identity(
                &mut armed.extreme_identity,
                &armed.extreme_id,
                armed_object.contains_key("extremeIdentity"),
                "extremeIdentity",
                "extreme",
                "extremeId",
            )?;
            validate_required_persisted_identity(
                &mut armed.swing_identity,
                &armed.swing_id,
                armed_object.contains_key("swingIdentity"),
                "swingIdentity",
                "swing",
                "swingId",
            )?;
            validate_optional_persisted_identity(
                &mut armed.divergence_identity,
                armed.divergence_id.as_ref(),
                armed_object.contains_key("divergenceIdentity"),
            )?;
        }
        return Ok(internal);
    }
    validate_shared_trade_fields(value, &LEGACY_FIELDS)?;
    Ok(RsiInternal {
        shared_trade: shared_trade_state_from_value(value)?,
        ..RsiInternal::default()
    })
}

fn validate_persisted_arm_fact_ids(value: &serde_json::Value) -> Result<(), PlanError> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        PlanError::Serialization("RSI persisted armed state must be an object or null".to_owned())
    })?;
    for (field, allow_null) in [
        ("extremeId", false),
        ("swingId", false),
        ("divergenceId", true),
    ] {
        if let Some(value) = object.get(field) {
            validate_persisted_fact_id_fields(value, field, allow_null)?;
        }
    }
    Ok(())
}

fn validate_persisted_fact_id_fields(
    value: &serde_json::Value,
    field: &str,
    allow_null: bool,
) -> Result<(), PlanError> {
    const FACT_ID_FIELDS: [&str; 7] = [
        "symbol",
        "timeframe",
        "kind",
        "confirmedAt",
        "ordinal",
        "algorithmVersion",
        "parameterVersion",
    ];
    if allow_null && value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        PlanError::Serialization(format!("RSI persisted {field} must be an object"))
    })?;
    let mut unknown_fields = object
        .keys()
        .filter(|candidate| !FACT_ID_FIELDS.contains(&candidate.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown_fields.sort();
    if unknown_fields.is_empty() {
        return Ok(());
    }
    Err(PlanError::Serialization(format!(
        "RSI persisted {field} contains unknown field(s): {}",
        unknown_fields.join(", ")
    )))
}

fn validate_required_persisted_identity(
    identity: &mut Option<StableFactIdentity>,
    id: &FactId,
    field_present: bool,
    identity_field: &str,
    identity_label: &str,
    id_field: &str,
) -> Result<(), PlanError> {
    if !field_present {
        *identity = Some(StableFactIdentity::from_fact_id(id));
        return Ok(());
    }
    let Some(identity) = identity.as_ref() else {
        return Err(PlanError::Serialization(format!(
            "RSI persisted {identity_field} cannot be null"
        )));
    };
    if identity.matches(id) {
        return Ok(());
    }
    Err(PlanError::Serialization(format!(
        "RSI persisted {identity_label} identity does not match {id_field}"
    )))
}

fn validate_optional_persisted_identity(
    identity: &mut Option<StableFactIdentity>,
    id: Option<&FactId>,
    field_present: bool,
) -> Result<(), PlanError> {
    if !field_present {
        *identity = id.map(StableFactIdentity::from_fact_id);
        return Ok(());
    }
    match (id, identity.as_ref()) {
        (None, None) => Ok(()),
        (Some(id), Some(identity)) if identity.matches(id) => Ok(()),
        (Some(_), Some(_)) => Err(PlanError::Serialization(
            "RSI persisted divergence identity does not match divergenceId".to_owned(),
        )),
        (Some(_), None) | (None, Some(_)) => Err(PlanError::Serialization(
            "RSI persisted divergenceId and divergenceIdentity must both be present or both be null"
                .to_owned(),
        )),
    }
}

fn validate_shared_trade_fields(
    value: &serde_json::Value,
    allowed_fields: &[&str],
) -> Result<(), PlanError> {
    let object = value.as_object().ok_or_else(|| {
        PlanError::Serialization("RSI shared trade state must be an object".to_owned())
    })?;
    let mut unknown_fields = object
        .keys()
        .filter(|field| !allowed_fields.contains(&field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown_fields.sort();
    if unknown_fields.is_empty() {
        return Ok(());
    }
    Err(PlanError::Serialization(format!(
        "RSI shared trade state contains unknown field(s): {}",
        unknown_fields.join(", ")
    )))
}

fn store_rsi_internal(
    state: &mut PlanRuntimeState,
    internal: &RsiInternal,
) -> Result<(), PlanError> {
    state.internal = serde_json::to_value(internal)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(())
}

fn consume_armed(internal: &mut RsiInternal) -> bool {
    consume_armed_through(internal, None)
}

fn consume_armed_through(internal: &mut RsiInternal, confirmed_through: Option<i64>) -> bool {
    let Some(armed) = internal.armed.take() else {
        return false;
    };
    let consumed_through = confirmed_through
        .unwrap_or(armed.extreme_id.confirmed_at)
        .max(armed.extreme_id.confirmed_at);
    internal.consumed_through = Some(
        internal
            .consumed_through
            .map_or(consumed_through, |consumed| consumed.max(consumed_through)),
    );
    true
}

fn rsi_observations(snapshot: &CanonicalMarketSnapshot) -> Result<Vec<RsiObservation>, PlanError> {
    snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Rsi)
        .map(|fact| {
            let value: RsiValue = serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok(RsiObservation {
                id: fact.id.clone(),
                value: value.value,
                close: value.close,
            })
        })
        .collect()
}

fn v5_rsi_observations(
    snapshot: &CanonicalMarketSnapshot,
) -> Result<Vec<RsiObservation>, PlanError> {
    let mut observations = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Rsi && fact.lifecycle == FactLifecycle::Confirmed)
        .map(|fact| {
            let value: RsiValue = serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))?;
            Ok(RsiObservation {
                id: fact.id.clone(),
                value: value.value,
                close: value.close,
            })
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    observations.sort_by_key(|observation| (observation.id.confirmed_at, observation.id.ordinal));
    Ok(observations)
}

fn evaluate_v5_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &RsiPlanParameters,
    internal: &mut RsiInternal,
) -> Result<Option<RsiSetup>, PlanError> {
    let observations = v5_rsi_observations(snapshot)?;
    let Some(latest) = observations.last().cloned() else {
        return Ok(None);
    };
    let lookback_ms = snapshot
        .target_timeframe
        .duration_ms()
        .checked_mul(i64::from(parameters.confirmed_swing_lookback))
        .ok_or_else(|| PlanError::Serialization("RSI swing lookback overflow".to_owned()))?;
    let confirms_arm = |armed: &RsiArmedSetup, observation: &RsiObservation| match armed.direction {
        Direction::Buy => {
            observation.value > armed.extreme_rsi && observation.close > armed.extreme_close
        }
        Direction::Sell => {
            observation.value < armed.extreme_rsi && observation.close < armed.extreme_close
        }
        Direction::None => false,
    };
    let mut completed_cycle: Option<(Direction, Decimal)> = None;
    let (armed, supporting_swing, current_extreme) = loop {
        for observation in &observations {
            if internal
                .consumed_through
                .is_some_and(|consumed| observation.id.confirmed_at <= consumed)
                || internal
                    .armed
                    .as_ref()
                    .is_some_and(|armed| observation.id.confirmed_at <= armed.armed_at)
            {
                continue;
            }
            let direction = if observation.value <= parameters.oversold_threshold {
                Direction::Buy
            } else if observation.value >= parameters.overbought_threshold {
                Direction::Sell
            } else {
                Direction::None
            };
            if direction == Direction::None {
                continue;
            }
            if internal.armed.is_none()
                && completed_cycle.is_some_and(|(completed_direction, completed_close)| {
                    direction == completed_direction
                        && match direction {
                            Direction::Buy => observation.close >= completed_close,
                            Direction::Sell => observation.close <= completed_close,
                            Direction::None => true,
                        }
                })
            {
                continue;
            }
            let should_replace = match internal.armed.as_ref() {
                None => true,
                Some(armed) if armed.direction != direction => true,
                Some(armed) if direction == Direction::Buy => observation.value < armed.extreme_rsi,
                Some(armed) if direction == Direction::Sell => {
                    observation.value > armed.extreme_rsi
                }
                Some(_) => false,
            };
            if !should_replace {
                continue;
            }
            let (swing_kind, fact_kind, divergence_kind) = if direction == Direction::Buy {
                (
                    SwingKind::Low,
                    FactKind::SwingLow,
                    DivergenceKind::RegularBullish,
                )
            } else {
                (
                    SwingKind::High,
                    FactKind::SwingHigh,
                    DivergenceKind::RegularBearish,
                )
            };
            let swing = typed_facts::<ConfirmedSwing>(&snapshot.target.facts, fact_kind)?
                .into_iter()
                .filter(|swing| {
                    swing.kind == swing_kind
                        && snapshot
                            .evaluated_closed_candle_time
                            .saturating_sub(swing.confirmed_at)
                            <= lookback_ms
                })
                .max_by_key(|swing| swing.confirmed_at);
            let Some(swing) = swing else {
                continue;
            };
            let swing_identity = StableFactIdentity::from_fact_id(&swing.id);
            let divergence =
                typed_facts::<RegularDivergence>(&snapshot.target.facts, FactKind::RsiDivergence)?
                    .into_iter()
                    .filter(|fact| {
                        fact.kind == divergence_kind
                            && swing_identity.matches(&fact.second_price_swing_id)
                            && fact.id.confirmed_at <= latest.id.confirmed_at
                    })
                    .max_by_key(|fact| fact.id.confirmed_at);
            if parameters.divergence_required && divergence.is_none() {
                continue;
            }
            let armed_at = observation.id.confirmed_at;
            let expires_at = armed_at
                .checked_add(
                    snapshot
                        .target_timeframe
                        .duration_ms()
                        .checked_mul(i64::from(parameters.reversal_confirmation_bars))
                        .ok_or_else(|| {
                            PlanError::Serialization("RSI confirmation window overflow".to_owned())
                        })?,
                )
                .ok_or_else(|| {
                    PlanError::Serialization("RSI confirmation expiry overflow".to_owned())
                })?;
            internal.armed = Some(RsiArmedSetup {
                direction,
                extreme_id: observation.id.clone(),
                extreme_identity: Some(StableFactIdentity::from_fact_id(&observation.id)),
                extreme_rsi: observation.value,
                extreme_close: observation.close,
                swing_id: swing.id.clone(),
                swing_identity: Some(swing_identity),
                divergence_id: divergence.as_ref().map(|fact| fact.id.clone()),
                divergence_identity: divergence
                    .as_ref()
                    .map(|fact| StableFactIdentity::from_fact_id(&fact.id)),
                armed_at,
                expires_at,
            });
            completed_cycle = None;
        }
        let Some(armed) = internal.armed.clone() else {
            return Ok(None);
        };
        let extreme_identity = armed
            .extreme_identity
            .clone()
            .unwrap_or_else(|| StableFactIdentity::from_fact_id(&armed.extreme_id));
        let mut matching_extremes = observations
            .iter()
            .filter(|observation| extreme_identity.matches(&observation.id));
        let Some(current_extreme) = matching_extremes.next().cloned() else {
            consume_armed(internal);
            return Ok(None);
        };
        if matching_extremes.next().is_some() {
            return Err(PlanError::Serialization(
                "ambiguous persisted RSI extreme identity".to_owned(),
            ));
        }
        let swing_kind = if armed.direction == Direction::Buy {
            FactKind::SwingLow
        } else {
            FactKind::SwingHigh
        };
        let swing_identity = armed
            .swing_identity
            .clone()
            .unwrap_or_else(|| StableFactIdentity::from_fact_id(&armed.swing_id));
        let mut matching_swings =
            typed_facts::<ConfirmedSwing>(&snapshot.target.facts, swing_kind)?
                .into_iter()
                .filter(|swing| swing_identity.matches(&swing.id));
        let Some(supporting_swing) = matching_swings.next() else {
            consume_armed(internal);
            return Ok(None);
        };
        if matching_swings.next().is_some() {
            return Err(PlanError::Serialization(
                "ambiguous persisted RSI swing identity".to_owned(),
            ));
        }
        if snapshot
            .evaluated_closed_candle_time
            .saturating_sub(supporting_swing.confirmed_at)
            > lookback_ms
        {
            consume_armed(internal);
            return Ok(None);
        }
        if let Some(historical_confirmation) =
            observations[..observations.len() - 1]
                .iter()
                .find(|observation| {
                    observation.id.confirmed_at > armed.armed_at
                        && observation.id.confirmed_at <= armed.expires_at
                        && confirms_arm(&armed, observation)
                })
        {
            completed_cycle = Some((armed.direction, historical_confirmation.close));
            consume_armed_through(internal, Some(historical_confirmation.id.confirmed_at));
            continue;
        }
        if latest.id.confirmed_at > armed.expires_at {
            completed_cycle = Some((armed.direction, armed.extreme_close));
            consume_armed_through(internal, Some(armed.expires_at));
            continue;
        }
        break (armed, supporting_swing, current_extreme);
    };
    if latest.id.confirmed_at <= armed.armed_at {
        return Ok(None);
    }
    if !confirms_arm(&armed, &latest) {
        return Ok(None);
    }
    let divergence_kind = match armed.direction {
        Direction::Buy => DivergenceKind::RegularBullish,
        Direction::Sell => DivergenceKind::RegularBearish,
        Direction::None => return Ok(None),
    };
    let divergence =
        typed_facts::<RegularDivergence>(&snapshot.target.facts, FactKind::RsiDivergence)?
            .into_iter()
            .filter(|fact| {
                fact.kind == divergence_kind
                    && armed
                        .swing_identity
                        .as_ref()
                        .is_some_and(|identity| identity.matches(&fact.second_price_swing_id))
                    && fact.id.confirmed_at <= latest.id.confirmed_at
            })
            .max_by_key(|fact| fact.id.confirmed_at);
    if parameters.divergence_required && divergence.is_none() {
        return Ok(None);
    }
    if let Some(current) = internal.armed.as_mut() {
        current.divergence_id = divergence.as_ref().map(|fact| fact.id.clone());
        current.divergence_identity = divergence
            .as_ref()
            .map(|fact| StableFactIdentity::from_fact_id(&fact.id));
    }
    let Some(atr_fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Atr)
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let atr = atr_fact
        .value
        .get("value")
        .cloned()
        .unwrap_or_else(|| atr_fact.value.clone());
    let atr =
        serde_json::from_value(atr).map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(Some(RsiSetup {
        direction: armed.direction,
        entered: current_extreme,
        exited: latest,
        swing: supporting_swing,
        divergence,
        atr_id: atr_fact.id.clone(),
        atr,
    }))
}

fn find_setup(
    snapshot: &CanonicalMarketSnapshot,
    parameters: &RsiPlanParameters,
) -> Result<Option<RsiSetup>, PlanError> {
    let observations = rsi_observations(snapshot)?;
    let Some(latest) = observations.last() else {
        return Ok(None);
    };
    let Some(pair) = observations
        .windows(2)
        .rev()
        .find(|pair| pair[1].id == latest.id)
    else {
        return Ok(None);
    };
    let direction = if pair[0].value <= parameters.oversold_threshold
        && pair[1].value > parameters.oversold_threshold
    {
        Direction::Buy
    } else if pair[0].value >= parameters.overbought_threshold
        && pair[1].value < parameters.overbought_threshold
    {
        Direction::Sell
    } else {
        return Ok(None);
    };
    let (swing_kind, fact_kind, divergence_kind) = if direction == Direction::Buy {
        (
            SwingKind::Low,
            FactKind::SwingLow,
            DivergenceKind::RegularBullish,
        )
    } else {
        (
            SwingKind::High,
            FactKind::SwingHigh,
            DivergenceKind::RegularBearish,
        )
    };
    let lookback_ms = snapshot
        .target_timeframe
        .duration_ms()
        .checked_mul(i64::from(parameters.confirmed_swing_lookback))
        .ok_or_else(|| PlanError::Serialization("RSI swing lookback overflow".to_owned()))?;
    let swing = typed_facts::<ConfirmedSwing>(&snapshot.target.facts, fact_kind)?
        .into_iter()
        .filter(|swing| {
            swing.kind == swing_kind
                && snapshot
                    .evaluated_closed_candle_time
                    .saturating_sub(swing.confirmed_at)
                    <= lookback_ms
        })
        .max_by_key(|swing| swing.confirmed_at);
    let Some(swing) = swing else {
        return Ok(None);
    };
    let divergence =
        typed_facts::<RegularDivergence>(&snapshot.target.facts, FactKind::RsiDivergence)?
            .into_iter()
            .filter(|fact| {
                fact.kind == divergence_kind
                    && fact.second_price_swing_id == swing.id
                    && fact.id.confirmed_at <= pair[1].id.confirmed_at
            })
            .max_by_key(|fact| fact.id.confirmed_at);
    if parameters.divergence_required && divergence.is_none() {
        return Ok(None);
    }
    let Some(atr_fact) = snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Atr)
        .max_by_key(|fact| fact.id.confirmed_at)
    else {
        return Ok(None);
    };
    let atr = atr_fact
        .value
        .get("value")
        .cloned()
        .unwrap_or_else(|| atr_fact.value.clone());
    let atr =
        serde_json::from_value(atr).map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(Some(RsiSetup {
        direction,
        entered: pair[0].clone(),
        exited: pair[1].clone(),
        swing,
        divergence,
        atr_id: atr_fact.id.clone(),
        atr,
    }))
}

fn build_signal(
    profile: &MaterializedProfile,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &RsiPlanParameters,
    setup: &RsiSetup,
) -> Result<Option<SignalDefinition>, PlanError> {
    let projected_entry = setup.exited.close;
    let buffer = setup.atr * parameters.atr_buffer_multiplier;
    let stop_loss = if setup.direction == Direction::Buy {
        setup.swing.price - buffer
    } else {
        setup.swing.price + buffer
    };
    let risk = (projected_entry - stop_loss).abs();
    if risk <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "RSI projected risk must be positive".to_owned(),
        ));
    }
    let primary_target = if setup.direction == Direction::Buy {
        projected_entry + risk * parameters.target_rr
    } else {
        projected_entry - risk * parameters.target_rr
    };
    if primary_target <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "RSI target must be positive".to_owned(),
        ));
    }
    let Some(execution_plan) = materialize_execution_plan(
        setup.direction,
        projected_entry,
        setup.swing.price,
        primary_target,
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
    let final_risk = (projected_entry - stop_loss).abs();
    if final_risk <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "RSI final risk must remain positive".to_owned(),
        ));
    }
    let risk_reward = ((primary_target - projected_entry).abs() / final_risk).round_dp(6);
    let evaluated = snapshot.evaluated_closed_candle_time;
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("RSI effective time overflow".to_owned()))?;
    let expires_at = effective_at.checked_add(
        profile
            .key
            .target_timeframe
            .duration_ms()
            .checked_mul(i64::from(parameters.max_bars_valid))
            .ok_or_else(|| PlanError::Serialization("RSI expiry overflow".to_owned()))?,
    );
    let signal_id = deterministic_uuid(&[
        profile.versions.release_id.as_bytes(),
        profile.key.symbol.as_str().as_bytes(),
        profile.key.target_timeframe.as_str().as_bytes(),
        b"RSI",
        &evaluated.to_be_bytes(),
    ]);
    let mut signal_evidence = vec![
        evidence(&setup.entered.id, "RSI_EXTREME_ENTERED"),
        evidence(&setup.exited.id, "RSI_REVERSAL_CONFIRMED"),
        evidence(&setup.swing.id, "CONFIRMED_SWING"),
        evidence(&setup.atr_id, "ATR_BUFFER"),
    ];
    if let Some(divergence) = &setup.divergence {
        signal_evidence.push(evidence(&divergence.id, "REGULAR_DIVERGENCE"));
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
    let reference_levels = extension_target
        .map(|price| {
            vec![ReferenceLevel {
                kind: "TP2_EXTENSION_TARGET".to_owned(),
                price,
            }]
        })
        .unwrap_or_default();
    Ok(Some(SignalDefinition {
        signal_id,
        symbol: profile.key.symbol.clone(),
        plan: PlanKind::Rsi,
        target_timeframe: profile.key.target_timeframe,
        evaluated_at: evaluated,
        direction: setup.direction,
        execution_instruction: ExecutionInstruction::NextOpen,
        entry_zone: None,
        projected_entry: Some(projected_entry),
        stop_loss,
        primary_target,
        execution_plan: Some(execution_plan),
        reference_levels,
        risk_reward,
        pre_entry_validation: None,
        confidence: Decimal::ONE,
        evidence: signal_evidence,
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
    parameters: &RsiPlanParameters,
    mut shared: crate::common::SharedTradeState,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    let signal = state
        .active_signal
        .clone()
        .ok_or(PlanError::NoActiveSignal)?;
    let evaluated = snapshot.evaluated_closed_candle_time;
    let latest_rsi = rsi_observations(snapshot)?
        .into_iter()
        .max_by_key(|observation| observation.id.confirmed_at);
    let latest_close = decimal_fact(snapshot, FactKind::MarketClose)?;
    let failed_reversal = state.trade_state == TradeState::Open
        && latest_rsi.as_ref().is_some_and(|rsi| {
            latest_close.is_some_and(|close| {
                shared
                    .entry_price
                    .is_some_and(|entry| match signal.direction {
                        Direction::Buy => {
                            rsi.value <= parameters.oversold_threshold && close < entry
                        }
                        Direction::Sell => {
                            rsi.value >= parameters.overbought_threshold && close > entry
                        }
                        Direction::None => false,
                    })
            })
        });
    let execution_plan_active = signal.execution_plan.is_some();
    let (kind, event_kind, reason) = if state.trade_state == TradeState::NotEntered
        && signal.expires_at.is_some_and(|expiry| evaluated >= expiry)
    {
        (
            DirectiveKind::ExpireEntry,
            Some(SignalEventKind::Expired),
            "RSI_ENTRY_EXPIRED",
        )
    } else if failed_reversal {
        (
            if execution_plan_active {
                DirectiveKind::EmergencyExit
            } else {
                DirectiveKind::SuggestClose
            },
            None,
            "RSI_FAILED_REVERSAL",
        )
    } else if state.trade_state == TradeState::Open
        && latest_rsi.as_ref().is_some_and(|rsi| {
            if signal.direction == Direction::Buy {
                rsi.value >= parameters.overbought_threshold
            } else {
                rsi.value <= parameters.oversold_threshold
            }
        })
    {
        (
            if execution_plan_active {
                DirectiveKind::EmergencyExit
            } else {
                DirectiveKind::SuggestClose
            },
            None,
            "RSI_OPPOSING_THRESHOLD",
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
                minimum_rr: parameters.target_rr,
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
        .ok_or_else(|| PlanError::Serialization("RSI directive time overflow".to_owned()))?;
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

fn manage_active_v5(
    mut state: PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &RsiPlanParameters,
    mut internal: RsiInternal,
    profit_protection: &analysis_domain::ProfitProtectionParameters,
    position_management: &analysis_domain::PositionManagementParameters,
) -> Result<PlanEvaluation, PlanError> {
    state.internal = serde_json::to_value(&internal.shared_trade)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    let mut evaluation = manage_active(
        state,
        snapshot,
        parameters,
        internal.shared_trade.clone(),
        profit_protection,
        position_management,
    )?;
    internal.shared_trade = shared_trade_state_from_value(&evaluation.next_state.internal)?;
    store_rsi_internal(&mut evaluation.next_state, &internal)?;
    Ok(evaluation)
}
