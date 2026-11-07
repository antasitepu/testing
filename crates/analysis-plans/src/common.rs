use analysis_core::StructureEvent;
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, CurrentPlanProjection, Direction, DirectiveKind, EvidenceRef,
    FactKind, HtfAlignment, HtfContext, ManagementDirective, PlanFeedback, PlanKind,
    PlanProfileKey, PositionManagementParameters, PriceZone, ProfitProtectionParameters, Readiness,
    ReadinessReason, SetupState, SignalDefinition, SignalEvent, SignalEventKind,
    SignalExecutionPlan, TargetLeg, TrackingHealth, TradeState, VersionIdentity,
};
use execution_domain::{PositionStage, TradeHealth};
use market_domain::Timeframe;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub trait PlanEvaluator: Send + Sync {
    fn kind(&self) -> PlanKind;
    fn algorithm_version(&self) -> &'static str;
    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRuntimeState {
    pub key: PlanProfileKey,
    pub versions: VersionIdentity,
    pub readiness: Readiness,
    pub setup_state: SetupState,
    pub direction: Direction,
    pub trade_state: TradeState,
    pub active_signal: Option<SignalDefinition>,
    pub active_virtual_trade_id: Option<Uuid>,
    #[serde(default)]
    pub position_stage: PositionStage,
    #[serde(default)]
    pub trade_health: TradeHealth,
    #[serde(default = "decimal_one")]
    pub remaining_fraction: Decimal,
    #[serde(default)]
    pub realized_r: Decimal,
    #[serde(default)]
    pub thesis_invalidation: Option<Decimal>,
    #[serde(default)]
    pub tp1_filled_at: Option<i64>,
    #[serde(default)]
    pub tp2_zone: Option<PriceZone>,
    pub next_event_sequence: i64,
    pub internal: Value,
}

impl PlanRuntimeState {
    pub fn neutral(profile: &MaterializedProfile) -> Self {
        Self {
            key: profile.key.clone(),
            versions: profile.versions.clone(),
            readiness: Readiness::Ready,
            setup_state: SetupState::Neutral,
            direction: Direction::None,
            trade_state: TradeState::NotEntered,
            active_signal: None,
            active_virtual_trade_id: None,
            position_stage: PositionStage::Legacy,
            trade_health: TradeHealth::Healthy,
            remaining_fraction: Decimal::ONE,
            realized_r: Decimal::ZERO,
            thesis_invalidation: None,
            tp1_filled_at: None,
            tp2_zone: None,
            next_event_sequence: 0,
            internal: Value::Null,
        }
    }

    pub fn not_ready(profile: &MaterializedProfile, reason: ReadinessReason) -> Self {
        let mut state = Self::neutral(profile);
        state.readiness = Readiness::NotReady { reason };
        state
    }

    pub fn clear_terminal_setup(&mut self) {
        if matches!(
            self.setup_state,
            SetupState::Invalidated | SetupState::Expired
        ) && self.trade_state == TradeState::NotEntered
        {
            self.active_signal = None;
            self.active_virtual_trade_id = None;
            self.direction = Direction::None;
            self.setup_state = SetupState::Neutral;
            self.reset_runner_lifecycle();
        }
    }

    fn reset_runner_lifecycle(&mut self) {
        self.position_stage = PositionStage::Legacy;
        self.trade_health = TradeHealth::Healthy;
        self.remaining_fraction = Decimal::ONE;
        self.realized_r = Decimal::ZERO;
        self.thesis_invalidation = None;
        self.tp1_filled_at = None;
        self.tp2_zone = None;
    }

    pub fn clear_terminal_trade(&mut self) {
        self.active_signal = None;
        self.active_virtual_trade_id = None;
        self.direction = Direction::None;
        self.setup_state = SetupState::Neutral;
        self.reset_runner_lifecycle();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanEvaluation {
    pub next_state: PlanRuntimeState,
    pub signal: Option<SignalDefinition>,
    pub events: Vec<SignalEvent>,
    pub directives: Vec<ManagementDirective>,
    pub rejected_candidates: Vec<RejectedCandidate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedCandidate {
    pub candidate_id: Uuid,
    pub reason: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateDecision {
    Accepted,
    Rejected(RejectedCandidate),
}

pub fn create_signal(
    state: &mut PlanRuntimeState,
    candidate: SignalDefinition,
) -> Result<CandidateDecision, PlanError> {
    validate_signal_identity(state, &candidate)?;
    if requires_execution_plan(&candidate.algorithm_version) && candidate.execution_plan.is_none() {
        return Ok(CandidateDecision::Rejected(RejectedCandidate {
            candidate_id: candidate.signal_id,
            reason: "MISSING_EXECUTION_PLAN".to_owned(),
            payload: serde_json::to_value(candidate)
                .map_err(|error| PlanError::Serialization(error.to_string()))?,
        }));
    }
    if !has_valid_price_geometry(&candidate) || candidate.validate_execution_geometry().is_err() {
        return Ok(CandidateDecision::Rejected(RejectedCandidate {
            candidate_id: candidate.signal_id,
            reason: "INVALID_PRICE_GEOMETRY".to_owned(),
            payload: serde_json::to_value(candidate)
                .map_err(|error| PlanError::Serialization(error.to_string()))?,
        }));
    }
    if let Some(active) = &state.active_signal {
        if active.signal_id == candidate.signal_id {
            if active == &candidate {
                return Ok(CandidateDecision::Accepted);
            }
            return Err(PlanError::ImmutableSignalRewrite);
        }
        return Ok(CandidateDecision::Rejected(RejectedCandidate {
            candidate_id: candidate.signal_id,
            reason: "ACTIVE_SLOT_OCCUPIED".to_owned(),
            payload: serde_json::to_value(candidate)
                .map_err(|error| PlanError::Serialization(error.to_string()))?,
        }));
    }
    state.setup_state = SetupState::EntryAvailable;
    state.direction = candidate.direction;
    state.trade_state = TradeState::NotEntered;
    state.active_signal = Some(candidate);
    state.active_virtual_trade_id = None;
    state.position_stage = PositionStage::Legacy;
    state.trade_health = TradeHealth::Healthy;
    state.remaining_fraction = Decimal::ONE;
    state.realized_r = Decimal::ZERO;
    state.thesis_invalidation = None;
    state.tp1_filled_at = None;
    state.tp2_zone = None;
    state.next_event_sequence = 0;
    Ok(CandidateDecision::Accepted)
}

fn requires_execution_plan(algorithm_version: &str) -> bool {
    matches!(
        algorithm_version,
        "ict-v3"
            | "smc-v3"
            | "smc-v4"
            | "rsi-v4"
            | "rsi-v5"
            | "qm-v3"
            | "qm-v4"
            | "mean-v3"
            | "trend-v3"
            | "trend-v4"
            | "ICT-runtime-v3"
            | "ICT-runtime-v4"
            | "SMC-runtime-v3"
            | "SMC-runtime-v4"
            | "RSI-runtime-v4"
            | "RSI-runtime-v5"
            | "QUASIMODO-runtime-v3"
            | "QUASIMODO-runtime-v4"
            | "MEAN_REVERSION-runtime-v3"
            | "TREND_BREAKOUT-runtime-v3"
            | "TREND_BREAKOUT-runtime-v4"
            | "squeeze-v1"
            | "VOLATILITY_SQUEEZE-runtime-v1"
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_execution_plan(
    direction: Direction,
    entry_reference: Decimal,
    thesis: Decimal,
    analytical_target: Decimal,
    atr: Decimal,
    plan_atr_buffer: Decimal,
    setup_zone_width: Decimal,
    price_tick: Decimal,
    parameters: &PositionManagementParameters,
) -> Result<Option<SignalExecutionPlan>, PlanError> {
    if direction == Direction::None
        || [
            entry_reference,
            thesis,
            analytical_target,
            atr,
            plan_atr_buffer,
            price_tick,
        ]
        .iter()
        .any(|value| *value <= Decimal::ZERO)
        || setup_zone_width < Decimal::ZERO
    {
        return Err(PlanError::Serialization(
            "execution plan inputs must have valid positive geometry".to_owned(),
        ));
    }

    let hard_stop_buffer = (atr * plan_atr_buffer)
        .max(setup_zone_width * parameters.zone_buffer_multiplier)
        .max(price_tick * Decimal::from(parameters.minimum_buffer_ticks));
    let target_buffer = (atr * parameters.target_buffer_atr_multiplier)
        .max(price_tick * Decimal::from(parameters.target_buffer_minimum_ticks));
    if hard_stop_buffer <= Decimal::ZERO || target_buffer <= Decimal::ZERO {
        return Err(PlanError::Serialization(
            "execution plan buffers must be positive".to_owned(),
        ));
    }

    let (hard_stop, tp2_low, tp2_high, candidate_tp1, tp2_edge) = match direction {
        Direction::Buy => {
            let hard_stop = thesis - hard_stop_buffer;
            let tp2_edge = analytical_target - target_buffer;
            let candidate_tp1 = entry_reference + (entry_reference - hard_stop) * parameters.tp1_r;
            (
                hard_stop,
                tp2_edge,
                analytical_target,
                candidate_tp1,
                tp2_edge,
            )
        }
        Direction::Sell => {
            let hard_stop = thesis + hard_stop_buffer;
            let tp2_edge = analytical_target + target_buffer;
            let candidate_tp1 = entry_reference - (hard_stop - entry_reference) * parameters.tp1_r;
            (
                hard_stop,
                analytical_target,
                tp2_edge,
                candidate_tp1,
                tp2_edge,
            )
        }
        Direction::None => unreachable!(),
    };

    let has_market_geometry = match direction {
        Direction::Buy => {
            hard_stop < thesis
                && thesis <= entry_reference
                && entry_reference < tp2_edge
                && tp2_edge <= analytical_target
        }
        Direction::Sell => {
            analytical_target <= tp2_edge
                && tp2_edge < entry_reference
                && entry_reference <= thesis
                && thesis < hard_stop
        }
        Direction::None => false,
    };
    if !has_market_geometry {
        return Ok(None);
    }

    let tp1_fits = match direction {
        Direction::Buy => candidate_tp1 <= tp2_edge,
        Direction::Sell => candidate_tp1 >= tp2_edge,
        Direction::None => false,
    };
    if !tp1_fits {
        return Ok(None);
    }

    let tp2_zone = PriceZone::new(tp2_low, tp2_high)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;

    let plan = SignalExecutionPlan {
        thesis_invalidation: thesis,
        hard_stop,
        tp1: TargetLeg {
            trigger_r: parameters.tp1_r,
            fraction: parameters.tp1_fraction,
        },
        tp2_zone,
    };
    plan.validate(direction, entry_reference, analytical_target)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(Some(plan))
}

fn has_valid_price_geometry(signal: &SignalDefinition) -> bool {
    let Some(entry) = signal_entry_reference(signal) else {
        return false;
    };
    match signal.direction {
        Direction::Buy => signal.stop_loss < entry && entry < signal.primary_target,
        Direction::Sell => signal.primary_target < entry && entry < signal.stop_loss,
        Direction::None => false,
    }
}

fn signal_entry_reference(signal: &SignalDefinition) -> Option<Decimal> {
    if let Some(entry) = signal.projected_entry {
        return Some(entry);
    }
    signal
        .entry_zone
        .as_ref()
        .map(|zone| (zone.low + zone.high) / Decimal::TWO)
}

pub fn append_event(state: &mut PlanRuntimeState, event: &SignalEvent) -> Result<(), PlanError> {
    let signal = state
        .active_signal
        .as_ref()
        .ok_or(PlanError::NoActiveSignal)?;
    if event.signal_id != signal.signal_id || event.sequence != state.next_event_sequence {
        return Err(PlanError::InvalidEventSequence);
    }
    state.next_event_sequence = state
        .next_event_sequence
        .checked_add(1)
        .ok_or(PlanError::InvalidEventSequence)?;
    match event.kind {
        SignalEventKind::Watching => state.setup_state = SetupState::Watching,
        SignalEventKind::EntryAvailable => state.setup_state = SetupState::EntryAvailable,
        SignalEventKind::Triggered => state.trade_state = TradeState::Open,
        SignalEventKind::Invalidated => state.setup_state = SetupState::Invalidated,
        SignalEventKind::Expired => state.setup_state = SetupState::Expired,
    }
    if let Some(virtual_trade_id) = event.virtual_trade_id {
        state.active_virtual_trade_id = Some(virtual_trade_id);
    }
    state.clear_terminal_setup();
    Ok(())
}

pub fn apply_terminal_event(
    state: &mut PlanRuntimeState,
    event: &SignalEvent,
) -> Result<(), PlanError> {
    let signal = state
        .active_signal
        .as_ref()
        .ok_or(PlanError::NoActiveSignal)?;
    if event.signal_id != signal.signal_id {
        return Err(PlanError::InvalidEventSequence);
    }
    match event.kind {
        SignalEventKind::Invalidated => state.setup_state = SetupState::Invalidated,
        SignalEventKind::Expired => state.setup_state = SetupState::Expired,
        _ => return Ok(()),
    }
    state.clear_terminal_setup();
    Ok(())
}

pub fn apply_feedback(
    state: &mut PlanRuntimeState,
    feedback: &PlanFeedback,
) -> Result<(), PlanError> {
    let active_signal_id = state
        .active_signal
        .as_ref()
        .map(|signal| signal.signal_id)
        .ok_or(PlanError::NoActiveSignal)?;
    match feedback {
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            ..
        } if *signal_id == active_signal_id => {
            if let Some(active_virtual_trade_id) = state.active_virtual_trade_id {
                if active_virtual_trade_id == *virtual_trade_id
                    && state.trade_state == TradeState::Open
                {
                    return Ok(());
                }
                return Err(PlanError::FeedbackIdentityMismatch);
            }
            state.trade_state = TradeState::Open;
            state.active_virtual_trade_id = Some(*virtual_trade_id);
            if let Some(execution_plan) = state
                .active_signal
                .as_ref()
                .and_then(|signal| signal.execution_plan.as_ref())
            {
                state.position_stage = PositionStage::FullPosition;
                state.trade_health = TradeHealth::Healthy;
                state.remaining_fraction = Decimal::ONE;
                state.realized_r = Decimal::ZERO;
                state.thesis_invalidation = Some(execution_plan.thesis_invalidation);
                state.tp1_filled_at = None;
                state.tp2_zone = Some(execution_plan.tp2_zone.clone());
            }
            Ok(())
        }
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            time,
            realized_fraction,
            remaining_fraction,
            ..
        } if *signal_id == active_signal_id
            && state.active_virtual_trade_id == Some(*virtual_trade_id) =>
        {
            if state.position_stage == PositionStage::FullPosition {
                let tp1_r = state
                    .active_signal
                    .as_ref()
                    .and_then(|signal| signal.execution_plan.as_ref())
                    .map(|plan| plan.tp1.trigger_r)
                    .unwrap_or(Decimal::ZERO);
                state.position_stage = PositionStage::Runner;
                state.trade_health = TradeHealth::Healthy;
                state.remaining_fraction = *remaining_fraction;
                state.realized_r = *realized_fraction * tp1_r;
                state.tp1_filled_at = Some(*time);
            }
            Ok(())
        }
        PlanFeedback::TradeClosed {
            signal_id,
            virtual_trade_id,
            ..
        } if *signal_id == active_signal_id
            && state.active_virtual_trade_id == Some(*virtual_trade_id) =>
        {
            state.trade_state = TradeState::Closed;
            state.active_virtual_trade_id = None;
            state.reset_runner_lifecycle();
            Ok(())
        }
        _ => Err(PlanError::FeedbackIdentityMismatch),
    }
}

const fn decimal_one() -> Decimal {
    Decimal::ONE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerHealthAction {
    ProtectProfit,
    SuggestClose,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunnerHealthInput {
    pub thesis_valid: bool,
    pub previous_close: Option<Decimal>,
    pub latest_close: Decimal,
    pub previous_mfe_r: Decimal,
    pub current_profit_r: Decimal,
    pub adverse_close_count: u32,
    pub htf_alignment: Option<HtfAlignment>,
    pub weakening_giveback_r: Decimal,
    pub reversal_giveback_r: Decimal,
    pub reversal_adverse_closes: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerHealthEvaluation {
    pub health: TradeHealth,
    pub action: Option<RunnerHealthAction>,
    pub new_mfe: bool,
}

pub fn evaluate_runner_health(input: RunnerHealthInput) -> RunnerHealthEvaluation {
    if !input.thesis_valid {
        return RunnerHealthEvaluation {
            health: TradeHealth::Invalid,
            action: None,
            new_mfe: false,
        };
    }
    let new_mfe = input.current_profit_r > input.previous_mfe_r;
    if new_mfe {
        return RunnerHealthEvaluation {
            health: TradeHealth::Healthy,
            action: None,
            new_mfe: true,
        };
    }
    let giveback_r = input.previous_mfe_r - input.current_profit_r;
    if input.current_profit_r > Decimal::ZERO
        && giveback_r >= input.reversal_giveback_r
        && input.adverse_close_count >= input.reversal_adverse_closes
    {
        return RunnerHealthEvaluation {
            health: TradeHealth::Weakening,
            action: Some(RunnerHealthAction::SuggestClose),
            new_mfe: false,
        };
    }
    let adverse_close = match (input.previous_close, input.htf_alignment) {
        (_, Some(HtfAlignment::Counter)) => true,
        (Some(previous), _) => input.latest_close != previous && input.adverse_close_count > 0,
        (None, _) => input.adverse_close_count > 0,
    };
    if giveback_r >= input.weakening_giveback_r || adverse_close {
        return RunnerHealthEvaluation {
            health: TradeHealth::Weakening,
            action: Some(RunnerHealthAction::ProtectProfit),
            new_mfe: false,
        };
    }
    RunnerHealthEvaluation {
        health: TradeHealth::Healthy,
        action: None,
        new_mfe: false,
    }
}

pub fn project_state(
    state: &PlanRuntimeState,
    evaluated_closed_candle_time: i64,
) -> CurrentPlanProjection {
    CurrentPlanProjection {
        symbol: state.key.symbol.clone(),
        plan: state.key.plan,
        target_timeframe: state.key.target_timeframe,
        readiness: state.readiness,
        setup_state: state.setup_state,
        direction: state.direction,
        trade_state: state.trade_state,
        tracking_health: TrackingHealth::Live,
        active_signal_id: state.active_signal.as_ref().map(|signal| signal.signal_id),
        active_virtual_trade_id: state.active_virtual_trade_id,
        version: state.versions.clone(),
        evaluated_closed_candle_time,
    }
}

pub const fn can_reverse(plan: PlanKind) -> bool {
    matches!(plan, PlanKind::TrendBreakout)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct SharedTradeState {
    pub entry_price: Option<Decimal>,
    pub initial_risk: Option<Decimal>,
    pub active_stop: Option<Decimal>,
    pub active_target: Option<Decimal>,
    pub best_closed_profit_r: Decimal,
    pub previous_close: Option<Decimal>,
    pub last_evaluated_close_time: Option<i64>,
    pub adverse_close_count: u32,
    pub profit_exit_requested_at: Option<i64>,
    pub new_mfe_on_last_close: bool,
}

pub(crate) fn shared_trade_state_from_value(value: &Value) -> Result<SharedTradeState, PlanError> {
    if value.is_null() {
        return Ok(SharedTradeState::default());
    }
    serde_json::from_value(value.clone())
        .map_err(|error| PlanError::Serialization(error.to_string()))
}

pub(crate) fn store_shared_trade_state(
    state: &mut PlanRuntimeState,
    shared: &SharedTradeState,
) -> Result<(), PlanError> {
    state.internal = serde_json::to_value(shared)
        .map_err(|error| PlanError::Serialization(error.to_string()))?;
    Ok(())
}

pub(crate) fn store_or_clear_shared_trade_state(
    state: &mut PlanRuntimeState,
    shared: &SharedTradeState,
) -> Result<(), PlanError> {
    if shared == &SharedTradeState::default() {
        state.internal = Value::Null;
        return Ok(());
    }
    store_shared_trade_state(state, shared)
}

pub(crate) fn apply_shared_trade_feedback(
    shared: &mut SharedTradeState,
    state: &PlanRuntimeState,
    feedback: &PlanFeedback,
) -> Result<(), PlanError> {
    let Some(signal) = state.active_signal.as_ref() else {
        return Ok(());
    };
    match feedback {
        PlanFeedback::ActualEntry {
            signal_id, price, ..
        } if *signal_id == signal.signal_id => {
            if shared.entry_price.is_some() {
                return Ok(());
            }
            let initial_risk = (*price - signal.stop_loss).abs();
            if initial_risk <= Decimal::ZERO {
                return Err(PlanError::Serialization(
                    "shared trade risk must be positive".to_owned(),
                ));
            }
            let active_target = signal
                .execution_plan
                .as_ref()
                .map(|plan| match signal.direction {
                    Direction::Buy => plan.tp2_zone.low,
                    Direction::Sell => plan.tp2_zone.high,
                    Direction::None => signal.primary_target,
                })
                .unwrap_or(signal.primary_target);
            *shared = SharedTradeState {
                entry_price: Some(*price),
                initial_risk: Some(initial_risk),
                active_stop: Some(signal.stop_loss),
                active_target: Some(active_target),
                ..SharedTradeState::default()
            };
        }
        PlanFeedback::TradeClosed { signal_id, .. } if *signal_id == signal.signal_id => {
            *shared = SharedTradeState::default();
        }
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            ..
        } if *signal_id == signal.signal_id
            && state.active_virtual_trade_id == Some(*virtual_trade_id) =>
        {
            if let Some(tp1_r) = signal
                .execution_plan
                .as_ref()
                .map(|plan| plan.tp1.trigger_r)
            {
                shared.best_closed_profit_r = shared.best_closed_profit_r.max(tp1_r);
            }
            if let Some(entry_price) = shared.entry_price {
                shared.active_stop = Some(match (signal.direction, shared.active_stop) {
                    (Direction::Buy, Some(stop)) => stop.max(entry_price),
                    (Direction::Sell, Some(stop)) => stop.min(entry_price),
                    (_, None) => entry_price,
                    (Direction::None, Some(stop)) => stop,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn shared_profit_protection_directive(
    state: &mut PlanRuntimeState,
    shared: &mut SharedTradeState,
    snapshot: &CanonicalMarketSnapshot,
    parameters: &ProfitProtectionParameters,
    position_management: &PositionManagementParameters,
) -> Result<Option<ManagementDirective>, PlanError> {
    if state.trade_state != TradeState::Open || state.position_stage != PositionStage::Runner {
        return Ok(None);
    }
    let Some(signal) = state.active_signal.as_ref() else {
        return Ok(None);
    };
    let Some(entry_price) = shared.entry_price else {
        return Ok(None);
    };
    let Some(initial_risk) = shared.initial_risk else {
        return Ok(None);
    };
    if initial_risk <= Decimal::ZERO {
        return Ok(None);
    }
    let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
        return Ok(None);
    };
    let current_profit_r = match signal.direction {
        Direction::Buy => latest_close - entry_price,
        Direction::Sell => entry_price - latest_close,
        Direction::None => return Ok(None),
    } / initial_risk;
    let evaluated = snapshot.evaluated_closed_candle_time;
    if shared.last_evaluated_close_time != Some(evaluated) {
        let previous_mfe_r = shared.best_closed_profit_r;
        shared.adverse_close_count = match (signal.direction, shared.previous_close) {
            (Direction::Buy, Some(previous)) if latest_close < previous => {
                shared.adverse_close_count.saturating_add(1)
            }
            (Direction::Sell, Some(previous)) if latest_close > previous => {
                shared.adverse_close_count.saturating_add(1)
            }
            _ => 0,
        };
        let health = evaluate_runner_health(RunnerHealthInput {
            thesis_valid: true,
            previous_close: shared.previous_close,
            latest_close,
            previous_mfe_r,
            current_profit_r,
            adverse_close_count: shared.adverse_close_count,
            htf_alignment: signal.htf_context.as_ref().map(|context| context.alignment),
            weakening_giveback_r: position_management.weakening_giveback_r,
            reversal_giveback_r: parameters.reversal_giveback_r,
            reversal_adverse_closes: parameters.adverse_closes,
        });
        shared.new_mfe_on_last_close = health.new_mfe;
        if health.new_mfe {
            shared.best_closed_profit_r = current_profit_r;
            shared.adverse_close_count = 0;
        }
        state.trade_health = health.health;
        shared.previous_close = Some(latest_close);
        shared.last_evaluated_close_time = Some(evaluated);
        if health.action == Some(RunnerHealthAction::SuggestClose)
            && shared.profit_exit_requested_at.is_none()
        {
            shared.profit_exit_requested_at = Some(evaluated);
            let reason = format!("{}_PROFIT_REVERSAL", management_reason_prefix(signal.plan));
            return management_directive(
                state,
                signal,
                evaluated,
                DirectiveKind::SuggestClose,
                None,
                &reason,
            )
            .map(Some);
        }
    } else {
        shared.new_mfe_on_last_close = false;
    }
    let activation_r = shared_profit_activation_r(signal, parameters);
    if shared.best_closed_profit_r < activation_r {
        return Ok(None);
    }
    let completed_steps =
        ((shared.best_closed_profit_r - activation_r) / parameters.lock_step_r).floor();
    let locked_profit_r = completed_steps * parameters.lock_step_r;
    let current_stop = shared.active_stop.unwrap_or(signal.stop_loss);
    let candidate = match signal.direction {
        Direction::Buy => entry_price + initial_risk * locked_profit_r,
        Direction::Sell => entry_price - initial_risk * locked_profit_r,
        Direction::None => return Ok(None),
    };
    let improved_and_behind_close = match signal.direction {
        Direction::Buy => candidate > current_stop && candidate < latest_close,
        Direction::Sell => candidate < current_stop && candidate > latest_close,
        Direction::None => false,
    };
    if !improved_and_behind_close {
        return Ok(None);
    }
    shared.active_stop = Some(candidate);
    let reason = if locked_profit_r == Decimal::ZERO {
        format!(
            "{}_SHARED_TRAIL_BREAKEVEN",
            management_reason_prefix(signal.plan)
        )
    } else {
        format!(
            "{}_STEPPED_PROFIT_LOCK",
            management_reason_prefix(signal.plan)
        )
    };
    management_directive(
        state,
        signal,
        evaluated,
        DirectiveKind::MoveStop,
        Some(candidate),
        &reason,
    )
    .map(Some)
}

pub(crate) fn shared_thesis_invalidation_directive(
    state: &mut PlanRuntimeState,
    snapshot: &CanonicalMarketSnapshot,
) -> Result<Option<ManagementDirective>, PlanError> {
    if state.trade_state != TradeState::Open {
        return Ok(None);
    }
    let Some(signal) = state.active_signal.clone() else {
        return Ok(None);
    };
    let Some(execution_plan) = signal.execution_plan.as_ref() else {
        return Ok(None);
    };
    let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
        return Ok(None);
    };
    let invalid = match signal.direction {
        Direction::Buy => latest_close < execution_plan.thesis_invalidation,
        Direction::Sell => latest_close > execution_plan.thesis_invalidation,
        Direction::None => false,
    };
    if !invalid {
        return Ok(None);
    }
    state.trade_health = TradeHealth::Invalid;
    let reason = format!(
        "{}_THESIS_INVALIDATED",
        management_reason_prefix(signal.plan)
    );
    management_directive(
        state,
        &signal,
        snapshot.evaluated_closed_candle_time,
        DirectiveKind::EmergencyExit,
        None,
        &reason,
    )
    .map(Some)
}

pub(crate) struct TargetExtensionRequest<'a> {
    pub plan_supplied_target: Option<Decimal>,
    pub position_management: &'a PositionManagementParameters,
    pub minimum_rr: Decimal,
}

pub(crate) fn immutable_extension_target(signal: &SignalDefinition) -> Option<Decimal> {
    signal
        .reference_levels
        .iter()
        .filter(|level| level.kind == "TP2_EXTENSION_TARGET")
        .map(|level| level.price)
        .filter(|price| match signal.direction {
            Direction::Buy => *price > signal.primary_target,
            Direction::Sell => *price < signal.primary_target,
            Direction::None => false,
        })
        .min_by_key(|price| (*price - signal.primary_target).abs())
}

pub(crate) fn shared_target_extension_directive(
    state: &mut PlanRuntimeState,
    shared: &mut SharedTradeState,
    snapshot: &CanonicalMarketSnapshot,
    request: TargetExtensionRequest<'_>,
) -> Result<Option<ManagementDirective>, PlanError> {
    if state.trade_state != TradeState::Open
        || state.position_stage != PositionStage::Runner
        || state.trade_health != TradeHealth::Healthy
        || !shared.new_mfe_on_last_close
    {
        return Ok(None);
    }
    let Some(signal) = state.active_signal.as_ref() else {
        return Ok(None);
    };
    let Some(structural_target) = request.plan_supplied_target else {
        return Ok(None);
    };
    let Some(entry_price) = shared.entry_price else {
        return Ok(None);
    };
    let Some(initial_risk) = shared.initial_risk else {
        return Ok(None);
    };
    let atr = decimal_fact(snapshot, FactKind::Atr)?.unwrap_or(Decimal::ZERO);
    let target_buffer = (atr * request.position_management.target_buffer_atr_multiplier).max(
        snapshot.price_tick
            * Decimal::from(request.position_management.target_buffer_minimum_ticks),
    );
    let Some(candidate) = buffered_extension_edge(
        signal.direction,
        structural_target,
        target_buffer,
        entry_price,
        initial_risk,
        request.minimum_rr,
    ) else {
        return Ok(None);
    };
    let Some(latest_close) = decimal_fact(snapshot, FactKind::MarketClose)? else {
        return Ok(None);
    };
    if !matches!(
        signal.htf_context.as_ref().map(|context| context.alignment),
        Some(HtfAlignment::Aligned | HtfAlignment::Neutral)
    ) {
        return Ok(None);
    }
    let current_target = shared.active_target.unwrap_or(signal.primary_target);
    let improves_target = match signal.direction {
        Direction::Buy => candidate > current_target && candidate > latest_close,
        Direction::Sell => candidate < current_target && candidate < latest_close,
        Direction::None => false,
    };
    if !improves_target {
        return Ok(None);
    }
    shared.active_target = Some(candidate);
    let target_reason = format!(
        "{}_SHARED_TARGET_EXTENSION",
        management_reason_prefix(signal.plan)
    );
    let evaluated = snapshot.evaluated_closed_candle_time;
    management_directive(
        state,
        signal,
        evaluated,
        DirectiveKind::MoveTarget,
        Some(candidate),
        &target_reason,
    )
    .map(Some)
}

fn buffered_extension_edge(
    direction: Direction,
    structural_target: Decimal,
    target_buffer: Decimal,
    entry_price: Decimal,
    initial_risk: Decimal,
    minimum_rr: Decimal,
) -> Option<Decimal> {
    if direction == Direction::None
        || structural_target <= Decimal::ZERO
        || target_buffer <= Decimal::ZERO
        || entry_price <= Decimal::ZERO
        || initial_risk <= Decimal::ZERO
        || minimum_rr <= Decimal::ZERO
    {
        return None;
    }
    let execution_edge = match direction {
        Direction::Buy => structural_target - target_buffer,
        Direction::Sell => structural_target + target_buffer,
        Direction::None => return None,
    };
    let reward = match direction {
        Direction::Buy => execution_edge - entry_price,
        Direction::Sell => entry_price - execution_edge,
        Direction::None => return None,
    };
    (execution_edge > Decimal::ZERO && reward / initial_risk >= minimum_rr)
        .then_some(execution_edge)
}

fn shared_profit_activation_r(
    signal: &SignalDefinition,
    parameters: &ProfitProtectionParameters,
) -> Decimal {
    match signal.htf_context.as_ref().map(|context| context.alignment) {
        Some(HtfAlignment::Counter) => parameters.counter_activation_r,
        _ => parameters.aligned_activation_r,
    }
}

fn management_reason_prefix(plan: PlanKind) -> &'static str {
    match plan {
        PlanKind::Ict => "ICT",
        PlanKind::Smc => "SMC",
        PlanKind::Rsi => "RSI",
        PlanKind::Quasimodo => "QM",
        PlanKind::TrendBreakout => "TREND",
        PlanKind::MeanReversion => "MEAN",
        PlanKind::SessionBreakout => "ORB",
        PlanKind::VolatilitySqueeze => "SQUEEZE",
    }
}

pub(crate) fn management_directive(
    state: &PlanRuntimeState,
    signal: &SignalDefinition,
    evaluated: i64,
    kind: DirectiveKind,
    price: Option<Decimal>,
    reason: &str,
) -> Result<ManagementDirective, PlanError> {
    let effective_at = evaluated
        .checked_add(1)
        .ok_or_else(|| PlanError::Serialization("management time overflow".to_owned()))?;
    Ok(ManagementDirective {
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
    })
}

pub(crate) fn deterministic_uuid(parts: &[&[u8]]) -> Uuid {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut high = OFFSET;
    let mut low = OFFSET ^ 0x9e3779b97f4a7c15;
    for part in parts {
        for byte in *part {
            high ^= u64::from(*byte);
            high = high.wrapping_mul(PRIME);
            low ^= u64::from(*byte).rotate_left(1);
            low = low.wrapping_mul(PRIME ^ 0x517cc1b727220a95);
        }
        high ^= 0xff;
        low ^= 0x7f;
    }
    Uuid::from_u128((u128::from(high) << 64) | u128::from(low))
}

#[cfg(test)]
mod extension_geometry_tests {
    use super::*;

    #[test]
    fn buffered_extension_edge_requires_configured_reward_for_buy_and_sell() {
        assert_eq!(
            buffered_extension_edge(
                Direction::Buy,
                Decimal::new(130, 0),
                Decimal::new(5, 0),
                Decimal::new(100, 0),
                Decimal::new(10, 0),
                Decimal::new(3, 0),
            ),
            None
        );
        assert_eq!(
            buffered_extension_edge(
                Direction::Sell,
                Decimal::new(70, 0),
                Decimal::new(5, 0),
                Decimal::new(100, 0),
                Decimal::new(10, 0),
                Decimal::new(3, 0),
            ),
            None
        );
    }

    #[test]
    fn buffered_extension_edge_returns_directional_execution_price() {
        assert_eq!(
            buffered_extension_edge(
                Direction::Buy,
                Decimal::new(140, 0),
                Decimal::new(5, 0),
                Decimal::new(100, 0),
                Decimal::new(10, 0),
                Decimal::new(3, 0),
            ),
            Some(Decimal::new(135, 0))
        );
        assert_eq!(
            buffered_extension_edge(
                Direction::Sell,
                Decimal::new(60, 0),
                Decimal::new(5, 0),
                Decimal::new(100, 0),
                Decimal::new(10, 0),
                Decimal::new(3, 0),
            ),
            Some(Decimal::new(65, 0))
        );
    }
}

pub(crate) fn typed_facts<T: DeserializeOwned>(
    facts: &[analysis_domain::ConfirmedFact<Value>],
    kind: FactKind,
) -> Result<Vec<T>, PlanError> {
    facts
        .iter()
        .filter(|fact| fact.id.kind == kind)
        .map(|fact| {
            serde_json::from_value(fact.value.clone())
                .map_err(|error| PlanError::Serialization(error.to_string()))
        })
        .collect()
}

pub(crate) fn decimal_fact(
    snapshot: &CanonicalMarketSnapshot,
    kind: FactKind,
) -> Result<Option<Decimal>, PlanError> {
    snapshot
        .target
        .facts
        .iter()
        .filter(|fact| fact.id.kind == kind)
        .max_by_key(|fact| fact.id.confirmed_at)
        .map(|fact| {
            let value = fact
                .value
                .get("value")
                .cloned()
                .unwrap_or_else(|| fact.value.clone());
            serde_json::from_value(value)
                .map_err(|error| PlanError::Serialization(error.to_string()))
        })
        .transpose()
}

pub(crate) fn evidence(id: &analysis_domain::FactId, reason: &str) -> EvidenceRef {
    EvidenceRef {
        fact_id: id.clone(),
        reason: reason.to_owned(),
    }
}

pub(crate) fn htf_context(
    snapshot: &CanonicalMarketSnapshot,
    signal_direction: Direction,
    configured_higher_timeframe: Option<Timeframe>,
) -> HtfContext {
    let Some((timeframe, facts)) = configured_higher_timeframe
        .and_then(|timeframe| {
            snapshot
                .higher_timeframes
                .iter()
                .find(|facts| facts.timeframe == timeframe)
                .map(|facts| (timeframe, facts))
        })
        .or_else(|| {
            snapshot
                .higher_timeframes
                .first()
                .map(|facts| (facts.timeframe, facts))
        })
    else {
        return HtfContext {
            higher_timeframe: configured_higher_timeframe,
            direction: Direction::None,
            alignment: HtfAlignment::Off,
            source: "OFF".to_owned(),
        };
    };

    let structure_direction = facts
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Structure)
        .filter_map(|fact| serde_json::from_value::<StructureEvent>(fact.value.clone()).ok())
        .max_by_key(|event| event.id.confirmed_at)
        .map(|event| event.direction);

    let (direction, source) = if let Some(direction) = structure_direction {
        (direction, "STRUCTURE")
    } else if let Some(direction) = facts
        .facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Ema)
        .filter_map(|fact| {
            let slope = fact.value.get("slope")?;
            serde_json::from_value::<Decimal>(slope.clone()).ok()
        })
        .next_back()
        .map(|slope| {
            if slope > Decimal::ZERO {
                Direction::Buy
            } else if slope < Decimal::ZERO {
                Direction::Sell
            } else {
                Direction::None
            }
        })
    {
        (direction, "EMA")
    } else {
        (Direction::None, "MISSING")
    };

    HtfContext {
        higher_timeframe: Some(timeframe),
        direction,
        alignment: htf_alignment(signal_direction, direction, source),
        source: source.to_owned(),
    }
}

fn htf_alignment(
    signal_direction: Direction,
    htf_direction: Direction,
    source: &str,
) -> HtfAlignment {
    if source == "MISSING" {
        return HtfAlignment::Missing;
    }
    if matches!(signal_direction, Direction::None) || matches!(htf_direction, Direction::None) {
        return HtfAlignment::Neutral;
    }
    if signal_direction == htf_direction {
        HtfAlignment::Aligned
    } else {
        HtfAlignment::Counter
    }
}

pub(crate) fn empty_evaluation(next_state: PlanRuntimeState) -> PlanEvaluation {
    PlanEvaluation {
        next_state,
        signal: None,
        events: Vec::new(),
        directives: Vec::new(),
        rejected_candidates: Vec::new(),
    }
}

fn validate_signal_identity(
    state: &PlanRuntimeState,
    signal: &SignalDefinition,
) -> Result<(), PlanError> {
    if signal.symbol != state.key.symbol
        || signal.plan != state.key.plan
        || signal.target_timeframe != state.key.target_timeframe
        || signal.release_id != state.versions.release_id
        || signal.algorithm_version != state.versions.algorithm_version
        || signal.parameter_version != state.versions.parameter_version
        || signal.direction == Direction::None
    {
        return Err(PlanError::SignalIdentityMismatch);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error("signal identity does not match runtime state")]
    SignalIdentityMismatch,
    #[error("immutable signal definition cannot be rewritten")]
    ImmutableSignalRewrite,
    #[error("event sequence or signal identity is invalid")]
    InvalidEventSequence,
    #[error("runtime has no active signal")]
    NoActiveSignal,
    #[error("feedback does not match the active signal/trade")]
    FeedbackIdentityMismatch,
    #[error("serialization failed: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::{management_reason_prefix, materialize_execution_plan};
    use analysis_domain::{Direction, PlanKind, PositionManagementParameters};
    use rust_decimal::Decimal;

    #[test]
    fn management_reason_prefixes_are_stable_for_all_plans() {
        assert_eq!(management_reason_prefix(PlanKind::Ict), "ICT");
        assert_eq!(management_reason_prefix(PlanKind::Smc), "SMC");
        assert_eq!(management_reason_prefix(PlanKind::Rsi), "RSI");
        assert_eq!(management_reason_prefix(PlanKind::Quasimodo), "QM");
        assert_eq!(management_reason_prefix(PlanKind::TrendBreakout), "TREND");
        assert_eq!(management_reason_prefix(PlanKind::MeanReversion), "MEAN");
        assert_eq!(management_reason_prefix(PlanKind::SessionBreakout), "ORB");
        assert_eq!(
            management_reason_prefix(PlanKind::VolatilitySqueeze),
            "SQUEEZE"
        );
    }

    #[test]
    fn execution_plan_rejects_market_candidate_with_thesis_beyond_entry() {
        let parameters = PositionManagementParameters::default();

        let buy = materialize_execution_plan(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_110, 0),
            Decimal::new(4_140, 0),
            Decimal::TEN,
            Decimal::new(2, 1),
            Decimal::ZERO,
            Decimal::new(1, 1),
            &parameters,
        );
        let sell = materialize_execution_plan(
            Direction::Sell,
            Decimal::new(4_100, 0),
            Decimal::new(4_090, 0),
            Decimal::new(4_060, 0),
            Decimal::TEN,
            Decimal::new(2, 1),
            Decimal::ZERO,
            Decimal::new(1, 1),
            &parameters,
        );

        assert_eq!(buy.unwrap(), None);
        assert_eq!(sell.unwrap(), None);
    }

    #[test]
    fn execution_plan_rejects_market_candidate_without_target_room() {
        let parameters = PositionManagementParameters::default();

        let result = materialize_execution_plan(
            Direction::Buy,
            Decimal::new(4_100, 0),
            Decimal::new(4_090, 0),
            Decimal::new(4_101, 0),
            Decimal::TEN,
            Decimal::new(2, 1),
            Decimal::ZERO,
            Decimal::new(1, 1),
            &parameters,
        );

        assert_eq!(result.unwrap(), None);
    }
}
