use crate::{
    AccountSimulationConfig, BacktestMetrics, BacktestTrade, ExecutionAdapter, ExecutionKind,
    FeeConfig, OhlcConservativeExecution, SimulatedExecution, SlippageConfig, SpreadConfig,
    SpreadConfigError, compute_metrics,
};
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, ManagementDirective, MaterializedProfile, PlanFeedback,
    PlanKind, SetupState, SignalDefinition, TradeState,
};
use analysis_plans::{PlanError, PlanEvaluator, PlanRuntimeState};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::ops::RangeInclusive;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunInput {
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub target_timeframe: Timeframe,
    pub start_time: i64,
    pub end_time: i64,
    pub release_id: Uuid,
    pub algorithm_version: String,
    pub parameter_version: String,
    pub fees: FeeConfig,
    pub slippage: SlippageConfig,
    pub account: Option<AccountSimulationConfig>,
    pub code_commit_hash: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayFrame {
    pub candle: Candle,
    pub snapshot: CanonicalMarketSnapshot,
    pub directives: Vec<ManagementDirective>,
    pub warmup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChronologicalSplit {
    pub train: RangeInclusive<i64>,
    pub validation: RangeInclusive<i64>,
    pub test: RangeInclusive<i64>,
}

impl ChronologicalSplit {
    pub fn new(
        train: RangeInclusive<i64>,
        validation: RangeInclusive<i64>,
        test: RangeInclusive<i64>,
    ) -> Result<Self, BacktestReplayError> {
        if train.start() > train.end()
            || validation.start() > validation.end()
            || test.start() > test.end()
            || train.end() >= validation.start()
            || validation.end() >= test.start()
        {
            return Err(BacktestReplayError::InvalidChronologicalSplit);
        }
        Ok(Self {
            train,
            validation,
            test,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestReplayResult {
    pub spread: SpreadConfig,
    pub input: BacktestRunInput,
    pub metrics: BacktestMetrics,
    pub trades: Vec<BacktestTrade>,
    pub executions: Vec<SimulatedExecution>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BacktestReplay {
    spread: SpreadConfig,
    input: BacktestRunInput,
    profile: MaterializedProfile,
}

impl BacktestReplay {
    pub fn new(input: BacktestRunInput, profile: MaterializedProfile) -> Self {
        Self {
            input,
            profile,
            spread: SpreadConfig::default(),
        }
    }

    /// Opt-in simulated bid/ask quotes. Fees and extra slippage remain separate;
    /// do not subtract a second spread from the resulting trade returns.
    pub fn with_spread(mut self, spread: SpreadConfig) -> Result<Self, SpreadConfigError> {
        self.spread = spread.validate()?;
        Ok(self)
    }

    pub fn run(
        &self,
        evaluator: &dyn PlanEvaluator,
        mut frames: Vec<ReplayFrame>,
    ) -> Result<BacktestReplayResult, BacktestReplayError> {
        frames.sort_by_key(|frame| frame.candle.open_time);
        self.run_ordered(evaluator, &frames)
    }

    pub fn run_ordered(
        &self,
        evaluator: &dyn PlanEvaluator,
        frames: &[ReplayFrame],
    ) -> Result<BacktestReplayResult, BacktestReplayError> {
        self.run_ordered_iter(evaluator, frames.iter(), frames.len() as u64)
    }

    pub fn run_ordered_iter<I, F>(
        &self,
        evaluator: &dyn PlanEvaluator,
        frames: I,
        frame_count: u64,
    ) -> Result<BacktestReplayResult, BacktestReplayError>
    where
        I: IntoIterator<Item = F>,
        F: Borrow<ReplayFrame>,
    {
        let mut session = self.start_session(evaluator)?;
        for frame in frames {
            session.process_frame(frame.borrow())?;
        }
        Ok(session.finish(frame_count))
    }

    pub fn start_session<'a>(
        &self,
        evaluator: &'a dyn PlanEvaluator,
    ) -> Result<BacktestReplaySession<'a>, BacktestReplayError> {
        validate_input(&self.input, &self.profile, evaluator)?;
        Ok(BacktestReplaySession {
            evaluator,
            input: self.input.clone(),
            profile: self.profile.clone(),
            spread: self.spread,
            execution: OhlcConservativeExecution::with_spread(self.spread)
                .map_err(|_| BacktestReplayError::InvalidSpread)?,
            state: PlanRuntimeState::neutral(&self.profile),
            pending_signals: Vec::new(),
            pending_directives: Vec::new(),
            executions: Vec::new(),
            trades: Vec::new(),
            open_trade: None,
            previous_open_time: None,
        })
    }
}

/// A setup that can no longer fill is terminal, not a closed trade. It gives its slot back through
/// the canonical reset, as the terminal signal event does at runtime.
fn release_unfilled_setup(state: &mut PlanRuntimeState, signal_id: Uuid, outcome: SetupState) {
    if state.trade_state == TradeState::NotEntered
        && state
            .active_signal
            .as_ref()
            .is_some_and(|active| active.signal_id == signal_id)
    {
        state.setup_state = outcome;
        state.clear_terminal_setup();
    }
}

pub struct BacktestReplaySession<'a> {
    spread: SpreadConfig,
    evaluator: &'a dyn PlanEvaluator,
    input: BacktestRunInput,
    profile: MaterializedProfile,
    execution: OhlcConservativeExecution,
    state: PlanRuntimeState,
    pending_signals: Vec<SignalDefinition>,
    pending_directives: Vec<ManagementDirective>,
    executions: Vec<SimulatedExecution>,
    trades: Vec<BacktestTrade>,
    open_trade: Option<OpenTrade>,
    previous_open_time: Option<i64>,
}

impl BacktestReplaySession<'_> {
    pub fn process_frame(&mut self, frame: &ReplayFrame) -> Result<(), BacktestReplayError> {
        if self
            .previous_open_time
            .is_some_and(|previous| previous > frame.candle.open_time)
        {
            return Err(BacktestReplayError::FramesOutOfOrder);
        }
        self.previous_open_time = Some(frame.candle.open_time);
        if frame.candle.open_time < self.input.start_time
            || frame.candle.open_time > self.input.end_time
        {
            return Ok(());
        }
        let mut feedback = Vec::new();
        let mut due_directives = frame.directives.clone();
        let mut future_directives = Vec::new();
        for directive in self.pending_directives.drain(..) {
            if directive.effective_at <= frame.candle.open_time {
                due_directives.push(directive);
            } else {
                future_directives.push(directive);
            }
        }
        self.pending_directives = future_directives;
        let cancelled_signal_ids: Vec<_> = due_directives
            .iter()
            .filter(|directive| {
                matches!(
                    directive.kind,
                    analysis_domain::DirectiveKind::CancelEntry
                        | analysis_domain::DirectiveKind::ExpireEntry
                )
            })
            .map(|directive| directive.signal_id)
            .collect();
        let mut still_pending = Vec::new();
        for signal in self.pending_signals.drain(..) {
            if signal
                .expires_at
                .is_some_and(|expires_at| frame.candle.open_time >= expires_at)
            {
                if self.open_trade.is_none() {
                    release_unfilled_setup(&mut self.state, signal.signal_id, SetupState::Expired);
                }
                continue;
            }
            if cancelled_signal_ids.contains(&signal.signal_id) {
                continue;
            }
            let signal_executions = self.execution.on_signal(&signal, &frame.candle);
            if signal_executions.is_empty() {
                still_pending.push(signal);
            } else {
                for item in signal_executions {
                    if item.kind == ExecutionKind::Entry {
                        let virtual_trade_id = deterministic_uuid(&[
                            item.signal_id.as_bytes(),
                            &item.time.to_be_bytes(),
                            b"entry",
                        ]);
                        feedback.push(PlanFeedback::ActualEntry {
                            signal_id: item.signal_id,
                            virtual_trade_id,
                            time: item.time,
                            price: item.price,
                        });
                        self.open_trade = Some(OpenTrade::new(
                            self.trades.len() as i64,
                            signal.clone(),
                            item.clone(),
                            virtual_trade_id,
                        ));
                    }
                    if item.kind == ExecutionKind::EntryCancelled && self.open_trade.is_none() {
                        release_unfilled_setup(
                            &mut self.state,
                            item.signal_id,
                            SetupState::Invalidated,
                        );
                    }
                    self.executions.push(item);
                }
            }
        }
        self.pending_signals = still_pending;

        if let Some(trade) = &mut self.open_trade {
            trade.observe(
                &self
                    .spread
                    .exit_candle(&frame.candle, trade.signal.direction),
            );
        }
        let exit_executions = self.execution.on_candle(&frame.candle, &due_directives);
        for item in exit_executions {
            if item.kind == ExecutionKind::PartialTakeProfit
                && let Some(trade) = &mut self.open_trade
            {
                trade.apply_partial(item.price);
                feedback.push(PlanFeedback::PartialTargetFilled {
                    signal_id: item.signal_id,
                    virtual_trade_id: trade.virtual_trade_id,
                    time: item.time,
                    price: item.price,
                    realized_fraction: Decimal::ONE - trade.remaining_fraction,
                    remaining_fraction: trade.remaining_fraction,
                });
            }
            if matches!(
                item.kind,
                ExecutionKind::StopLoss
                    | ExecutionKind::TakeProfit
                    | ExecutionKind::ExitAtNextOpen
                    | ExecutionKind::SuggestClose
                    | ExecutionKind::EmergencyExit
                    | ExecutionKind::ReverseExit
            ) && let Some(trade) = self.open_trade.take()
            {
                let virtual_trade_id = trade.virtual_trade_id;
                let closed = trade.close(item.clone(), self.input.fees, self.input.slippage);
                feedback.push(PlanFeedback::TradeClosed {
                    signal_id: closed.signal_id,
                    virtual_trade_id,
                    time: closed.exit_time,
                    reason: closed.exit_reason.clone(),
                });
                self.trades.push(closed);
            }
            self.executions.push(item);
        }

        if frame.warmup {
            return Ok(());
        }
        let evaluation = self
            .evaluator
            .evaluate(&self.profile, &frame.snapshot, &self.state, &feedback)
            .map_err(BacktestReplayError::Evaluation)?;
        self.state = evaluation.next_state;
        if let Some(signal) = evaluation.signal {
            self.pending_signals.push(signal);
        }
        self.pending_directives
            .extend(evaluation.directives.iter().cloned());
        Ok(())
    }

    pub fn finish(self, frame_count: u64) -> BacktestReplayResult {
        let metrics = compute_metrics(
            self.trades.clone(),
            self.input.start_time,
            self.input.end_time,
            frame_count,
            self.input.fees,
            self.input.slippage,
        );
        BacktestReplayResult {
            spread: self.spread,
            input: self.input,
            metrics,
            trades: self.trades,
            executions: self.executions,
        }
    }
}

#[derive(Clone, Debug)]
struct OpenTrade {
    sequence: i64,
    signal: SignalDefinition,
    entry: SimulatedExecution,
    virtual_trade_id: Uuid,
    mfe_r: Decimal,
    mae_r: Decimal,
    realized_r: Decimal,
    remaining_fraction: Decimal,
    partial_execution_count: u32,
}

impl OpenTrade {
    fn new(
        sequence: i64,
        signal: SignalDefinition,
        entry: SimulatedExecution,
        virtual_trade_id: Uuid,
    ) -> Self {
        Self {
            sequence,
            signal,
            entry,
            virtual_trade_id,
            mfe_r: Decimal::ZERO,
            mae_r: Decimal::ZERO,
            realized_r: Decimal::ZERO,
            remaining_fraction: Decimal::ONE,
            partial_execution_count: 0,
        }
    }

    fn apply_partial(&mut self, price: Decimal) {
        let risk = risk(&self.signal, self.entry.price);
        if risk.is_zero() {
            return;
        }
        let fraction = self
            .signal
            .execution_plan
            .as_ref()
            .map(|plan| plan.tp1.fraction)
            .unwrap_or(Decimal::ZERO);
        let partial_r = if self.signal.direction == Direction::Buy {
            (price - self.entry.price) / risk
        } else {
            (self.entry.price - price) / risk
        };
        self.realized_r = (self.realized_r + fraction * partial_r).round_dp(6);
        self.remaining_fraction = (self.remaining_fraction - fraction).max(Decimal::ZERO);
        self.partial_execution_count += 1;
    }

    fn observe(&mut self, candle: &Candle) {
        let risk = risk(&self.signal, self.entry.price);
        if risk.is_zero() {
            return;
        }
        let favorable = if self.signal.direction == Direction::Buy {
            candle.high - self.entry.price
        } else {
            self.entry.price - candle.low
        };
        let adverse = if self.signal.direction == Direction::Buy {
            candle.low - self.entry.price
        } else {
            self.entry.price - candle.high
        };
        self.mfe_r = self.mfe_r.max(favorable / risk);
        self.mae_r = self.mae_r.min(adverse / risk);
    }

    fn close(
        self,
        exit: SimulatedExecution,
        fees: FeeConfig,
        slippage: SlippageConfig,
    ) -> BacktestTrade {
        let risk = risk(&self.signal, self.entry.price);
        let runner_r = if risk.is_zero() {
            Decimal::ZERO
        } else if self.signal.direction == Direction::Buy {
            (exit.price - self.entry.price) / risk
        } else {
            (self.entry.price - exit.price) / risk
        };
        let runner_contribution_r = (self.remaining_fraction * runner_r).round_dp(6);
        let gross_r = (self.realized_r + runner_contribution_r).round_dp(6);
        let hard_stop_counterfactual_r = (self.realized_r - self.remaining_fraction).round_dp(6);
        let tp2_execution_edge_r = self.signal.execution_plan.as_ref().and_then(|plan| {
            if risk.is_zero() {
                return None;
            }
            let edge = match self.signal.direction {
                Direction::Buy => plan.tp2_zone.low,
                Direction::Sell => plan.tp2_zone.high,
                Direction::None => return None,
            };
            Some(match self.signal.direction {
                Direction::Buy => (edge - self.entry.price) / risk,
                Direction::Sell => (self.entry.price - edge) / risk,
                Direction::None => Decimal::ZERO,
            })
        });
        let charged_execution_count = Decimal::from(self.partial_execution_count + 1);
        let fees_r = fees.per_execution_r * charged_execution_count;
        let slippage_r = slippage.per_execution_r * charged_execution_count;
        BacktestTrade {
            trade_id: deterministic_uuid(&[
                self.signal.signal_id.as_bytes(),
                &self.entry.time.to_be_bytes(),
                &exit.time.to_be_bytes(),
                b"trade",
            ]),
            signal_id: self.signal.signal_id,
            sequence: self.sequence,
            symbol: self.signal.symbol,
            plan: self.signal.plan,
            timeframe: self.signal.target_timeframe,
            direction: self.signal.direction,
            htf_context: self.signal.htf_context,
            entry_time: self.entry.time,
            exit_time: exit.time,
            entry_price: self.entry.price,
            exit_price: exit.price,
            gross_r,
            net_r: gross_r - fees_r - slippage_r,
            mfe_r: self.mfe_r,
            mae_r: self.mae_r,
            duration_ms: exit.time - self.entry.time,
            exit_reason: exit.kind.label().to_owned(),
            fees_r,
            slippage_r,
            ambiguous_intrabar: exit.ambiguous_intrabar,
            partial_filled: self.partial_execution_count > 0,
            realized_r: self.realized_r,
            runner_contribution_r: Some(runner_contribution_r),
            hard_stop_counterfactual_r: Some(hard_stop_counterfactual_r),
            tp2_execution_edge_r,
            initial_risk_price: Some(risk),
        }
    }
}

fn validate_input(
    input: &BacktestRunInput,
    profile: &MaterializedProfile,
    evaluator: &dyn PlanEvaluator,
) -> Result<(), BacktestReplayError> {
    if input.start_time > input.end_time {
        return Err(BacktestReplayError::InvalidRange);
    }
    if input.symbol != profile.key.symbol
        || input.plan != profile.key.plan
        || input.target_timeframe != profile.key.target_timeframe
        || input.release_id != profile.versions.release_id
        || input.algorithm_version != profile.versions.algorithm_version
        || input.parameter_version != profile.versions.parameter_version
        || input.plan != evaluator.kind()
        || input.algorithm_version != evaluator.algorithm_version()
    {
        return Err(BacktestReplayError::IdentityMismatch);
    }
    Ok(())
}

fn risk(signal: &SignalDefinition, entry_price: Decimal) -> Decimal {
    (entry_price - signal.stop_loss).abs()
}

fn deterministic_uuid(parts: &[&[u8]]) -> Uuid {
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

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum BacktestReplayError {
    #[error("spread price must be non-negative")]
    InvalidSpread,
    #[error("backtest chronological split is invalid")]
    InvalidChronologicalSplit,
    #[error("backtest start time must be before end time")]
    InvalidRange,
    #[error("backtest input, profile, and evaluator identity must match")]
    IdentityMismatch,
    #[error("backtest replay frames must be ordered chronologically")]
    FramesOutOfOrder,
    #[error("plan evaluation failed: {0}")]
    Evaluation(PlanError),
}

impl BacktestReplayError {
    pub const fn partial_result(&self) -> Option<&BacktestReplayResult> {
        None
    }
}

trait ExecutionKindName {
    fn label(self) -> &'static str;
}

impl ExecutionKindName for ExecutionKind {
    fn label(self) -> &'static str {
        match self {
            ExecutionKind::Entry => "ENTRY",
            ExecutionKind::ExitAtNextOpen => "EXIT_AT_NEXT_OPEN",
            ExecutionKind::SuggestClose => "SUGGEST_CLOSE",
            ExecutionKind::EmergencyExit => "EMERGENCY_EXIT",
            ExecutionKind::ReverseExit => "REVERSE_EXIT",
            ExecutionKind::ReverseEntry => "REVERSE_ENTRY",
            ExecutionKind::StopLoss => "STOP_LOSS",
            ExecutionKind::TakeProfit => "TAKE_PROFIT",
            ExecutionKind::PartialTakeProfit => "PARTIAL_TAKE_PROFIT",
            ExecutionKind::EntryCancelled => "ENTRY_CANCELLED",
        }
    }
}
