use crate::{
    ExecutionError, TrackerInput, TrackerRuntime, TrackerRuntimeCheckpoint, TrackerRuntimeError,
    TrackerStore,
};
use analysis_domain::{DirectiveKind, ManagementDirective, PlanProfileKey, SignalDefinition};
use execution_domain::MarketCursor;
use execution_domain::TrackingHealth;
use execution_domain::VirtualTrade;
use market_domain::{Candle, MarketEvent, Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

pub struct ExecutionSupervisor {
    tracker: TrackerRuntime,
    tracker_store: Arc<dyn TrackerStore>,
    health: HashMap<Symbol, TrackingHealth>,
    claimed_symbols: HashSet<Symbol>,
    last_seen_cursor: Option<MarketCursor>,
    last_checkpointed_cursor: Option<MarketCursor>,
    cancelled: bool,
}

struct RuntimeCheckpoint {
    tracker: TrackerRuntimeCheckpoint,
    last_seen_cursor: Option<MarketCursor>,
    last_checkpointed_cursor: Option<MarketCursor>,
}

impl ExecutionSupervisor {
    pub fn new(tracker_store: Arc<dyn TrackerStore>) -> Self {
        Self {
            tracker: TrackerRuntime::default(),
            tracker_store,
            health: HashMap::new(),
            claimed_symbols: HashSet::new(),
            last_seen_cursor: None,
            last_checkpointed_cursor: None,
            cancelled: false,
        }
    }

    pub fn claim_symbol(&mut self, symbol: Symbol) -> bool {
        self.claimed_symbols.insert(symbol)
    }

    pub fn health(&self, symbol: &Symbol) -> Option<TrackingHealth> {
        self.health.get(symbol).copied()
    }

    pub fn active_trade(&self, key: &PlanProfileKey) -> Option<&VirtualTrade> {
        self.tracker.active_trade(key)
    }

    pub fn last_seen_cursor(&self) -> Option<&MarketCursor> {
        self.last_seen_cursor.as_ref()
    }

    pub async fn restore_active(&mut self, symbol: &Symbol) -> Result<(), ExecutionError> {
        let snapshot = self.tracker_store.load_active(symbol).await?;
        let cursor = snapshot.cursor.clone();
        let replaying = self.tracker.replaying_trades();
        let delayed_opens = self.tracker.delayed_next_opens();
        let market_closures = self.tracker.market_closures();
        self.tracker = TrackerRuntime::default();
        self.tracker.restore_snapshot(snapshot);
        self.tracker.resume_replay(replaying);
        self.tracker.resume_delayed_next_opens(delayed_opens);
        self.tracker.set_market_closures(market_closures);
        self.last_seen_cursor = cursor.clone();
        self.last_checkpointed_cursor = cursor;
        self.health.insert(symbol.clone(), TrackingHealth::Live);
        Ok(())
    }

    pub fn begin_replay(&mut self, signal_id: Uuid, through: i64) {
        self.tracker.begin_replay(signal_id, through);
    }

    pub fn delay_next_open(&mut self, signal_id: Uuid, open_time: i64) {
        self.tracker.delay_next_open(signal_id, open_time);
    }

    pub fn set_market_closures(&mut self, closures: Vec<(i64, i64)>) {
        self.tracker.set_market_closures(closures);
    }

    pub fn replay_cursor(&self) -> Option<i64> {
        self.tracker.replay_cursor()
    }

    pub fn replaying_trade_ids(&self) -> Vec<Uuid> {
        self.tracker.replaying_trades().into_keys().collect()
    }

    pub fn finish_replay(&mut self, latest_close_time: i64) {
        self.tracker.finish_replay(latest_close_time);
    }

    pub async fn checkpoint_market_cursor(&mut self) -> Result<SupervisorStep, ExecutionError> {
        let Some(cursor) = self.last_seen_cursor.clone() else {
            return Ok(SupervisorStep::Ignored);
        };
        if self
            .last_checkpointed_cursor
            .as_ref()
            .is_some_and(|checkpoint| {
                checkpoint.symbol == cursor.symbol
                    && checkpoint.last_processed_agg_trade_id >= cursor.last_processed_agg_trade_id
            })
        {
            return Ok(SupervisorStep::Ignored);
        }
        match self.tracker_store.checkpoint_cursor(cursor.clone()).await {
            Ok(()) => {
                self.health
                    .insert(cursor.symbol.clone(), TrackingHealth::Live);
                self.last_checkpointed_cursor = Some(cursor);
                Ok(SupervisorStep::Committed)
            }
            Err(error) => {
                tracing::warn!(%error, symbol = %cursor.symbol, "execution cursor checkpoint failed");
                self.health
                    .insert(cursor.symbol.clone(), TrackingHealth::Stale);
                Ok(SupervisorStep::Frozen)
            }
        }
    }

    pub async fn rebase_market_cursor(
        &mut self,
        cursor: MarketCursor,
    ) -> Result<SupervisorStep, ExecutionError> {
        if self.last_seen_cursor.as_ref().is_some_and(|seen| {
            seen.symbol == cursor.symbol
                && seen.last_processed_agg_trade_id >= cursor.last_processed_agg_trade_id
        }) {
            return Ok(SupervisorStep::Ignored);
        }
        self.health
            .insert(cursor.symbol.clone(), TrackingHealth::Reconciling);
        self.last_seen_cursor = Some(cursor);
        self.checkpoint_market_cursor().await
    }

    pub async fn process(
        &mut self,
        event: SupervisorEvent,
    ) -> Result<SupervisorStep, ExecutionError> {
        if self.cancelled {
            return Ok(SupervisorStep::Cancelled);
        }
        match event {
            SupervisorEvent::Cancel => {
                self.cancelled = true;
                Ok(SupervisorStep::Cancelled)
            }
            SupervisorEvent::SignalOpened(signal) => {
                let symbol = signal.symbol.clone();
                let checkpoint = self.runtime_checkpoint();
                let transition = self
                    .tracker
                    .try_apply(TrackerInput::RecommendationOpened(*signal))
                    .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                self.commit(symbol, transition, checkpoint).await
            }
            SupervisorEvent::Directive(directive) => {
                let tracked_symbol = self.tracker.tracked_symbol_for_directive(&directive);
                if tracked_symbol
                    .as_ref()
                    .is_some_and(|symbol| self.health.get(symbol) == Some(&TrackingHealth::Stale))
                {
                    return Ok(SupervisorStep::Frozen);
                }
                let checkpoint = self.runtime_checkpoint();
                let input = reverse_at_next_open_input(&directive)
                    .unwrap_or(TrackerInput::Directive(directive));
                let transition = match self.tracker.try_apply(input) {
                    Ok(transition) => transition,
                    Err(TrackerRuntimeError::UnknownDirective) => {
                        return Ok(SupervisorStep::Ignored);
                    }
                    Err(error) => return Err(ExecutionError::InvalidData(error.to_string())),
                };
                let symbol = transition
                    .tracker_state
                    .as_ref()
                    .map(|state| state.symbol.clone())
                    .or_else(|| {
                        transition
                            .virtual_trade_write
                            .as_ref()
                            .map(|write| write.trade.symbol.clone())
                    })
                    .or(tracked_symbol);
                if let Some(symbol) = symbol {
                    self.commit(symbol, transition, checkpoint).await
                } else {
                    Ok(SupervisorStep::Ignored)
                }
            }
            SupervisorEvent::Market(MarketEvent::AggregateTrade(trade)) => {
                if self.health.get(&trade.symbol) == Some(&TrackingHealth::Stale) {
                    return Ok(SupervisorStep::Frozen);
                }
                if let Some(cursor) = self
                    .last_seen_cursor
                    .as_ref()
                    .filter(|cursor| cursor.symbol == trade.symbol)
                {
                    if trade.aggregate_trade_id <= cursor.last_processed_agg_trade_id {
                        return Ok(SupervisorStep::Ignored);
                    }
                    if trade.aggregate_trade_id != cursor.last_processed_agg_trade_id + 1 {
                        return Ok(SupervisorStep::GapDetected);
                    }
                }
                let symbol = trade.symbol.clone();
                let checkpoint = self.runtime_checkpoint();
                let transition = self
                    .tracker
                    .try_apply(TrackerInput::AggregateTrade(trade))
                    .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                if let Some(cursor) = transition.market_cursor.clone() {
                    self.last_seen_cursor = Some(cursor);
                }
                if !transition.needs_commit() {
                    return Ok(SupervisorStep::Ignored);
                }
                self.commit(symbol, transition, checkpoint).await
            }
            SupervisorEvent::Market(MarketEvent::ClosedCandle(candle)) => {
                if self.health.get(&candle.symbol) == Some(&TrackingHealth::Stale) {
                    return Ok(SupervisorStep::Frozen);
                }
                let symbol = candle.symbol.clone();
                let checkpoint = self.runtime_checkpoint();
                let canonical = self
                    .tracker
                    .try_apply(TrackerInput::CanonicalOpen(candle.clone()))
                    .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                let mut step = SupervisorStep::Ignored;
                if canonical.needs_commit() {
                    step = self.commit(symbol.clone(), canonical, checkpoint).await?;
                    if step == SupervisorStep::Frozen {
                        return Ok(step);
                    }
                }
                if candle.timeframe == Timeframe::M1 {
                    let checkpoint = self.runtime_checkpoint();
                    let reconciliation = self
                        .tracker
                        .try_apply(TrackerInput::OhlcReconciliation(candle))
                        .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                    if reconciliation.needs_commit() {
                        step = self.commit(symbol, reconciliation, checkpoint).await?;
                    }
                    if step != SupervisorStep::Frozen {
                        let checkpoint = self.checkpoint_market_cursor().await?;
                        if checkpoint == SupervisorStep::Frozen {
                            return Ok(checkpoint);
                        }
                    }
                }
                Ok(step)
            }
            SupervisorEvent::ReplayCandle(candle) => {
                if self.health.get(&candle.symbol) == Some(&TrackingHealth::Stale) {
                    return Ok(SupervisorStep::Frozen);
                }
                let symbol = candle.symbol.clone();
                let checkpoint = self.runtime_checkpoint();
                let transition = self
                    .tracker
                    .try_apply(TrackerInput::ReplayCandle(candle))
                    .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                self.commit(symbol, transition, checkpoint).await
            }
            SupervisorEvent::ReferencePrice {
                symbol,
                time,
                price,
            } => {
                if self.health.get(&symbol) == Some(&TrackingHealth::Stale) {
                    return Ok(SupervisorStep::Frozen);
                }
                let checkpoint = self.runtime_checkpoint();
                let transition = self
                    .tracker
                    .try_apply(TrackerInput::ReferencePrice {
                        symbol: symbol.clone(),
                        time,
                        price,
                    })
                    .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
                if !transition.needs_commit() {
                    return Ok(SupervisorStep::Ignored);
                }
                self.commit(symbol, transition, checkpoint).await
            }
            SupervisorEvent::Market(_) => Ok(SupervisorStep::Ignored),
        }
    }

    async fn commit(
        &mut self,
        symbol: Symbol,
        transition: crate::DurableTransition,
        checkpoint: RuntimeCheckpoint,
    ) -> Result<SupervisorStep, ExecutionError> {
        if !transition.needs_commit() {
            return Ok(SupervisorStep::Ignored);
        }
        let cursor = transition.market_cursor.clone();
        match self.tracker_store.commit_transition(transition).await {
            Ok(_) => {
                if let Some(cursor) = cursor {
                    self.last_seen_cursor = Some(cursor.clone());
                    self.last_checkpointed_cursor = Some(cursor);
                }
                self.health.insert(symbol, TrackingHealth::Live);
                Ok(SupervisorStep::Committed)
            }
            Err(error) => {
                tracing::warn!(%error, %symbol, "execution tracker transition failed");
                self.rollback_runtime(checkpoint);
                self.health.insert(symbol, TrackingHealth::Stale);
                Ok(SupervisorStep::Frozen)
            }
        }
    }

    fn runtime_checkpoint(&self) -> RuntimeCheckpoint {
        RuntimeCheckpoint {
            tracker: self.tracker.checkpoint(),
            last_seen_cursor: self.last_seen_cursor.clone(),
            last_checkpointed_cursor: self.last_checkpointed_cursor.clone(),
        }
    }

    fn rollback_runtime(&mut self, checkpoint: RuntimeCheckpoint) {
        self.tracker.rollback(checkpoint.tracker);
        self.last_seen_cursor = checkpoint.last_seen_cursor;
        self.last_checkpointed_cursor = checkpoint.last_checkpointed_cursor;
    }
}

/// When analysis attaches a validated opposite setup, execute reverse as one
/// atomic close+open transition at the next-open fill price.
fn reverse_at_next_open_input(directive: &ManagementDirective) -> Option<TrackerInput> {
    if directive.kind != DirectiveKind::ReverseAtNextOpen {
        return None;
    }
    let reverse_signal = directive.reverse_signal.clone()?;
    let price = directive.price?;
    let open_time = directive.effective_at;
    let close_time = open_time
        .checked_add(
            reverse_signal
                .target_timeframe
                .duration_ms()
                .saturating_sub(1),
        )
        .unwrap_or(open_time);
    let candle = Candle::new(
        reverse_signal.symbol.clone(),
        reverse_signal.target_timeframe,
        open_time,
        close_time,
        price,
        price,
        price,
        price,
    )
    .ok()?;
    Some(TrackerInput::ReverseAtNextOpen {
        directive: directive.clone(),
        reverse_signal,
        canonical_open: candle,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum SupervisorEvent {
    Market(MarketEvent),
    SignalOpened(Box<SignalDefinition>),
    Directive(ManagementDirective),
    ReplayCandle(Candle),
    ReferencePrice {
        symbol: Symbol,
        time: i64,
        price: Decimal,
    },
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStep {
    Committed,
    Frozen,
    GapDetected,
    Ignored,
    Cancelled,
}
