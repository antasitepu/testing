use crate::reconciliation::{reconciliation_event_id, reconciliation_trade_id};
use crate::{
    AmbiguityDecision, AmbiguousOutcome, DurableTransition, ExecutionKind, NewMonitoringEvent,
    NextOpenDecision, OhlcExecutionRange, PendingNextOpen, TrackerInput, TrackerSnapshot,
    TrackerState, VirtualTradeWrite, classify_ohlc_ambiguity, proves_effective_zone_touch,
};
use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanFeedback,
    PlanProfileKey, SignalDefinition, SignalEvent, SignalEventKind,
};
use execution_domain::{
    Ambiguity, ExecutionModel, PositionStage, ReconciliationSource, TrackingHealth, TradeHealth,
    TradeState, VirtualTrade, VirtualTradeEvent, VirtualTradeEventKind,
};
use market_domain::{AggregateTrade, Candle, Symbol};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const EXECUTION_PLAN_RR_FAILED: &str = "EXECUTION_PLAN_RR_FAILED";

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TrackerRuntimeError {
    #[error("management directive does not match an active tracked signal or trade")]
    UnknownDirective,
    #[error("signal cannot be converted into a virtual trade: {0}")]
    InvalidSignal(String),
}

#[derive(Clone, Debug, Default)]
pub struct TrackerRuntime {
    recommendations: HashMap<PlanProfileKey, SignalDefinition>,
    trades: HashMap<PlanProfileKey, VirtualTrade>,
    health: HashMap<Symbol, TrackingHealth>,
    entry_feedback_sent: HashSet<Uuid>,
    partial_feedback_sent: HashSet<Uuid>,
    close_feedback_sent: HashSet<Uuid>,
    next_trade_sequence: HashMap<Uuid, i64>,
    /// Trades opened from history, with the close time of the last candle each has seen.
    /// Live market data skips them until they have caught up.
    replaying: HashMap<Uuid, i64>,
    /// Next-open setups raised before a market closure, with the open they wait for.
    delayed_opens: HashMap<Uuid, i64>,
    /// Known market closures as `[start, end)`, sorted by start.
    market_closures: Vec<(i64, i64)>,
}

#[derive(Clone, Debug)]
pub struct TrackerRuntimeCheckpoint(TrackerRuntime);

impl TrackerRuntime {
    pub fn checkpoint(&self) -> TrackerRuntimeCheckpoint {
        TrackerRuntimeCheckpoint(self.clone())
    }

    pub fn rollback(&mut self, checkpoint: TrackerRuntimeCheckpoint) {
        *self = checkpoint.0;
    }

    pub fn restore_snapshot(&mut self, snapshot: TrackerSnapshot) {
        let sequences: HashMap<Uuid, i64> = snapshot
            .active_trade_sequences
            .into_iter()
            .map(|item| (item.virtual_trade_id, item.next_sequence.max(1)))
            .collect();
        for signal in snapshot.active_recommendations {
            let key = key_for_signal(&signal);
            if self
                .trades
                .get(&key)
                .is_none_or(|trade| trade.trade_state.is_terminal())
            {
                self.recommendations.insert(key, signal.clone());
            }
            self.health
                .insert(signal.symbol.clone(), TrackingHealth::Live);
        }
        for trade in snapshot.active_trades {
            if trade.trade_state.is_terminal() {
                continue;
            }
            let key = PlanProfileKey {
                symbol: trade.symbol.clone(),
                plan: trade.plan,
                target_timeframe: trade.timeframe,
            };
            self.health
                .insert(trade.symbol.clone(), TrackingHealth::Live);
            self.entry_feedback_sent.insert(trade.virtual_trade_id);
            if trade.position_stage == PositionStage::Runner || trade.tp1_filled_at.is_some() {
                self.partial_feedback_sent.insert(trade.virtual_trade_id);
            }
            self.next_trade_sequence.insert(
                trade.virtual_trade_id,
                sequences.get(&trade.virtual_trade_id).copied().unwrap_or(1),
            );
            self.trades.insert(key, trade);
        }
        for state in snapshot.tracker_states {
            self.health.insert(state.symbol, state.tracking_health);
        }
    }

    /// Holds a trade back from live data because later candles already exist and must reach it
    /// first, in order. `through` is the close time of the last candle the trade has seen.
    pub fn begin_replay(&mut self, signal_id: Uuid, through: i64) {
        if let Some(trade) = self
            .trades
            .values()
            .find(|trade| trade.signal_id == signal_id && trade.trade_state == TradeState::Open)
        {
            self.replaying.insert(trade.virtual_trade_id, through);
        }
    }

    /// The close time of the last candle seen by the trade furthest behind.
    pub fn replay_cursor(&self) -> Option<i64> {
        self.trades
            .values()
            .filter(|trade| trade.trade_state == TradeState::Open)
            .filter_map(|trade| self.replaying.get(&trade.virtual_trade_id).copied())
            .min()
    }

    pub fn replaying_trades(&self) -> HashMap<Uuid, i64> {
        self.replaying.clone()
    }

    /// Carries replay progress across a restore. Only trades still open keep replaying.
    pub fn resume_replay(&mut self, replaying: HashMap<Uuid, i64>) {
        let open: HashSet<Uuid> = self
            .trades
            .values()
            .filter(|trade| trade.trade_state == TradeState::Open)
            .map(|trade| trade.virtual_trade_id)
            .collect();
        self.replaying = replaying
            .into_iter()
            .filter(|(id, _)| open.contains(id))
            .collect();
    }

    /// Makes a next-open setup raised before a market closure wait for `open_time`.
    pub fn delay_next_open(&mut self, signal_id: Uuid, open_time: i64) {
        self.delayed_opens.insert(signal_id, open_time);
    }

    pub fn delayed_next_opens(&self) -> HashMap<Uuid, i64> {
        self.delayed_opens.clone()
    }

    /// Carries delays across a restore. Only setups still waiting keep theirs.
    pub fn resume_delayed_next_opens(&mut self, delayed: HashMap<Uuid, i64>) {
        let waiting: HashSet<Uuid> = self
            .recommendations
            .values()
            .map(|signal| signal.signal_id)
            .collect();
        self.delayed_opens = delayed
            .into_iter()
            .filter(|(signal_id, _)| waiting.contains(signal_id))
            .collect();
    }

    /// Replaces the known market closures. Time without a known closure counts as open.
    pub fn set_market_closures(&mut self, mut closures: Vec<(i64, i64)>) {
        closures.sort_unstable();
        self.market_closures = closures;
    }

    pub fn market_closures(&self) -> Vec<(i64, i64)> {
        self.market_closures.clone()
    }

    /// Waiting setups ignore prices inside a closure, as the backtest does by skipping those
    /// bars. Open trades do not use this: they are tracked through a closure.
    fn market_closed(&self, time: i64) -> bool {
        let after = self
            .market_closures
            .partition_point(|(start, _)| *start <= time);
        after > 0 && time < self.market_closures[after - 1].1
    }

    /// Before this time a setup cannot fill and prices do not judge it.
    fn entry_boundary(&self, signal: &SignalDefinition) -> i64 {
        self.delayed_opens
            .get(&signal.signal_id)
            .copied()
            .unwrap_or(signal.effective_at)
    }

    /// Returns trades that have seen every stored candle to live data.
    pub fn finish_replay(&mut self, latest_close_time: i64) {
        self.replaying
            .retain(|_, through| *through < latest_close_time);
        let unfinished = std::mem::take(&mut self.replaying);
        self.resume_replay(unfinished);
    }

    pub fn active_recommendation(&self, key: &PlanProfileKey) -> Option<&SignalDefinition> {
        self.recommendations.get(key)
    }

    pub fn active_trade(&self, key: &PlanProfileKey) -> Option<&VirtualTrade> {
        self.trades.get(key)
    }

    pub fn tracked_symbol_for_directive(&self, directive: &ManagementDirective) -> Option<Symbol> {
        self.recommendations
            .values()
            .find(|signal| signal.signal_id == directive.signal_id)
            .map(|signal| signal.symbol.clone())
            .or_else(|| {
                self.trades
                    .values()
                    .find(|trade| {
                        directive.virtual_trade_id == Some(trade.virtual_trade_id)
                            || trade.signal_id == directive.signal_id
                    })
                    .map(|trade| trade.symbol.clone())
            })
    }

    pub fn health(&self, symbol: Symbol) -> Option<TrackingHealth> {
        self.health.get(&symbol).copied()
    }

    pub fn apply(&mut self, input: TrackerInput) -> DurableTransition {
        self.try_apply(input).expect("tracker input must be valid")
    }

    pub fn try_apply(
        &mut self,
        input: TrackerInput,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        match input {
            TrackerInput::RecommendationOpened(signal) => self.open_recommendation(signal),
            TrackerInput::AggregateTrade(trade) => self.apply_trade(trade),
            TrackerInput::Directive(directive) => self.apply_directive(directive),
            TrackerInput::ReverseAtNextOpen {
                directive,
                reverse_signal,
                canonical_open,
            } => self.apply_reverse(directive, reverse_signal, canonical_open),
            TrackerInput::SignalEvent(event) => self.apply_signal_event(event),
            TrackerInput::CanonicalOpen(candle) => self.apply_canonical_open(candle),
            TrackerInput::ReferencePrice {
                symbol,
                time,
                price,
            } => self.apply_terminal_price(symbol, time, time, price, None),
            TrackerInput::OhlcReconciliation(candle) => {
                self.apply_ohlc_reconciliation(candle, false)
            }
            TrackerInput::ReplayCandle(candle) => self.apply_replay_candle(candle),
            TrackerInput::SessionChanged(_) => Ok(DurableTransition::default()),
        }
    }

    fn open_recommendation(
        &mut self,
        signal: SignalDefinition,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        let key = key_for_signal(&signal);
        self.health
            .insert(signal.symbol.clone(), TrackingHealth::Live);
        if !has_valid_signal_geometry(&signal) {
            return Ok(self.close_recommendation_before_entry(
                key,
                signal.clone(),
                signal.effective_at,
                signal_entry_reference(&signal).unwrap_or(signal.stop_loss),
                RecommendationExit::Invalidated("invalid price geometry"),
            ));
        }
        // An open trade owns the plan/timeframe slot. Do not emit a phantom
        // ENTRY_AVAILABLE recommendation or RECOMMENDATION_OPENED event that
        // would reappear after restart while the trade is still open.
        if self
            .trades
            .get(&key)
            .is_some_and(|trade| !trade.trade_state.is_terminal())
        {
            return Ok(DurableTransition::default());
        }
        let replaced = self.recommendations.insert(key, signal.clone());
        let mut transition = DurableTransition {
            tracker_state: Some(tracker_state(&signal, TrackingHealth::Live, None)),
            monitoring_events: vec![monitoring_event(
                "RECOMMENDATION_OPENED",
                "signal",
                Some(signal.signal_id),
                signal_recommendation_payload(&signal),
            )],
            ..DurableTransition::default()
        };
        if let Some(replaced) = replaced {
            transition.tracker_states.push(TrackerState {
                signal_id: replaced.signal_id,
                symbol: replaced.symbol.clone(),
                state: json!({
                    "signalId": replaced.signal_id,
                    "setupState": "INVALIDATED",
                    "tradeState": TradeState::NotEntered,
                    "reason": "replaced by newer recommendation"
                }),
                tracking_health: TrackingHealth::Live,
            });
            transition.signal_events.push(SignalEvent {
                event_id: Uuid::now_v7(),
                signal_id: replaced.signal_id,
                sequence: 1,
                kind: SignalEventKind::Invalidated,
                effective_at: signal.effective_at,
                recorded_at: signal.detected_at,
                virtual_trade_id: None,
                price: signal_entry_reference(&replaced),
                reason: Some("replaced by newer recommendation".to_owned()),
            });
            transition.monitoring_events.push(monitoring_event(
                "RECOMMENDATION_INVALIDATED",
                "signal",
                Some(replaced.signal_id),
                recommendation_terminal_payload(
                    &replaced,
                    "INVALIDATED",
                    "replaced by newer recommendation",
                    signal_entry_reference(&replaced),
                    Some(signal.signal_id),
                ),
            ));
        }
        Ok(transition)
    }

    fn apply_trade(
        &mut self,
        trade: AggregateTrade,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        let cursor = execution_domain::MarketCursor {
            symbol: trade.symbol.clone(),
            last_processed_agg_trade_id: trade.aggregate_trade_id,
            last_trade_time: trade.trade_time,
        };
        // Every recommendation this trade touches enters on this tick, in a stable
        // order. Entries are built before any state changes so an invalid signal
        // cannot leave the tick half-applied. `None` marks an RR rejection.
        let mut touched: Vec<_> = self
            .recommendations
            .iter()
            .filter(|(_, signal)| {
                signal.symbol == trade.symbol
                    && signal.execution_instruction == ExecutionInstruction::LimitZone
                    && signal.effective_at <= trade.trade_time
                    && !self.market_closed(trade.trade_time)
                    && signal.entry_zone.as_ref().is_some_and(|zone| {
                        proves_effective_zone_touch(zone, &trade, signal.effective_at, false)
                    })
            })
            .map(|(key, signal)| (key.clone(), signal.clone()))
            .collect();
        touched.sort_by_key(|(key, _)| (key.plan, key.target_timeframe));
        let mut entries = Vec::with_capacity(touched.len());
        for (key, signal) in touched {
            let entry = match virtual_trade_from_signal(&signal, Uuid::now_v7(), &trade) {
                Ok(virtual_trade) => Some(virtual_trade),
                Err(TrackerRuntimeError::InvalidSignal(reason))
                    if reason == EXECUTION_PLAN_RR_FAILED =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            entries.push((key, signal, entry));
        }

        // Exits of other plans on the same tick must not suppress these entries.
        let mut transition = self.apply_terminal_price(
            trade.symbol.clone(),
            trade.trade_time,
            trade.event_time,
            trade.price,
            Some(trade.aggregate_trade_id),
        )?;
        for (key, signal, entry) in entries {
            let still_open = self
                .recommendations
                .get(&key)
                .is_some_and(|current| current.signal_id == signal.signal_id);
            let slot_taken = self
                .trades
                .get(&key)
                .is_some_and(|active| !active.trade_state.is_terminal());
            if !still_open || slot_taken {
                continue;
            }
            let step = match entry {
                Some(virtual_trade) => self.open_virtual_trade(key, &signal, virtual_trade, &trade),
                None => self.close_recommendation_before_entry(
                    key,
                    signal,
                    trade.trade_time,
                    trade.price,
                    RecommendationExit::Invalidated(EXECUTION_PLAN_RR_FAILED),
                ),
            };
            transition.absorb(step);
        }
        transition.market_cursor = Some(cursor);
        Ok(transition)
    }

    fn open_virtual_trade(
        &mut self,
        key: PlanProfileKey,
        signal: &SignalDefinition,
        virtual_trade: VirtualTrade,
        trade: &AggregateTrade,
    ) -> DurableTransition {
        let virtual_trade_id = virtual_trade.virtual_trade_id;
        self.recommendations.remove(&key);
        self.trades.insert(key, virtual_trade.clone());
        self.forget_replaced_trades();
        self.next_trade_sequence.insert(virtual_trade_id, 2);
        let feedback = self.entry_feedback(&virtual_trade);
        DurableTransition {
            tracker_state: Some(tracker_state(
                signal,
                TrackingHealth::Live,
                Some(&virtual_trade),
            )),
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: virtual_trade.clone(),
            }),
            trade_events: vec![VirtualTradeEvent {
                event_id: Uuid::now_v7(),
                virtual_trade_id,
                sequence: 1,
                kind: VirtualTradeEventKind::EntryTriggered,
                event_time: trade.trade_time,
                payload: json!({
                    "aggregateTradeId": trade.aggregate_trade_id,
                    "price": trade.price
                }),
            }],
            signal_events: vec![SignalEvent {
                event_id: Uuid::now_v7(),
                signal_id: signal.signal_id,
                sequence: 1,
                kind: SignalEventKind::Triggered,
                effective_at: trade.trade_time,
                recorded_at: trade.event_time,
                virtual_trade_id: Some(virtual_trade_id),
                price: Some(trade.price),
                reason: Some("entry zone touched".to_owned()),
            }],
            plan_feedback: feedback.into_iter().collect(),
            monitoring_events: vec![monitoring_event(
                "VIRTUAL_TRADE_OPENED",
                "virtual_trade",
                Some(virtual_trade_id),
                virtual_trade_payload(&virtual_trade),
            )],
            ..DurableTransition::default()
        }
    }

    fn apply_terminal_price(
        &mut self,
        symbol: market_domain::Symbol,
        trade_time: i64,
        event_time: i64,
        price: Decimal,
        aggregate_trade_id: Option<i64>,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        for active in self.trades.values_mut().filter(|active| {
            active.symbol == symbol
                && active.trade_state == TradeState::Open
                && trade_time >= active.actual_entry_time
                && !self.replaying.contains_key(&active.virtual_trade_id)
        }) {
            update_point_excursion(active, price);
        }
        let partial_trades = self
            .trades
            .iter()
            .filter_map(|(key, active)| {
                if active.symbol != symbol
                    || active.trade_state != TradeState::Open
                    || trade_time < active.actual_entry_time
                    || self.replaying.contains_key(&active.virtual_trade_id)
                    || stop_touched(active, price)
                    || !partial_target_touched(active, price)
                {
                    return None;
                }
                Some((key.clone(), active.clone()))
            })
            .collect::<Vec<_>>();
        let terminal_trades = self
            .trades
            .iter()
            .filter_map(|(key, active)| {
                if active.symbol != symbol
                    || active.trade_state != TradeState::Open
                    || self.replaying.contains_key(&active.virtual_trade_id)
                {
                    return None;
                }
                if trade_time < active.actual_entry_time {
                    return None;
                }
                if partial_target_touched(active, price) && !stop_touched(active, price) {
                    return None;
                }
                exit_level(active, price).map(|exit| (key.clone(), active.clone(), exit))
            })
            .collect::<Vec<_>>();
        let terminal_recommendations = self
            .recommendations
            .iter()
            .filter_map(|(key, signal)| {
                if signal.symbol != symbol || self.entry_boundary(signal) > trade_time {
                    return None;
                }
                // Prices inside a closure do not judge a setup, but its time still runs out.
                if self.market_closed(trade_time)
                    && signal
                        .expires_at
                        .is_none_or(|expires_at| trade_time < expires_at)
                {
                    return None;
                }
                recommendation_exit(signal, price, trade_time)
                    .map(|exit| (key.clone(), signal.clone(), exit))
            })
            .collect::<Vec<_>>();
        if !partial_trades.is_empty()
            || !terminal_trades.is_empty()
            || !terminal_recommendations.is_empty()
        {
            let mut transition = DurableTransition {
                ..DurableTransition::default()
            };
            if let Some(aggregate_trade_id) = aggregate_trade_id {
                transition.market_cursor = Some(execution_domain::MarketCursor {
                    symbol: symbol.clone(),
                    last_processed_agg_trade_id: aggregate_trade_id,
                    last_trade_time: trade_time,
                });
            }
            let mut first_trade_write = true;
            let mut first_tracker_state = true;
            for (key, active) in partial_trades {
                let runner = activate_runner(active, trade_time)?;
                let partial_price = runner.tp1_price.expect("FULL_POSITION trade must have TP1");
                let partial_feedback = self.partial_feedback(&runner, trade_time, partial_price);
                let partial_sequence = self.next_sequence(runner.virtual_trade_id);
                let runner_sequence = self.next_sequence(runner.virtual_trade_id);
                transition.trade_events.extend([
                    VirtualTradeEvent {
                        event_id: Uuid::now_v7(),
                        virtual_trade_id: runner.virtual_trade_id,
                        sequence: partial_sequence,
                        kind: VirtualTradeEventKind::PartialTargetTouched,
                        event_time: trade_time,
                        payload: json!({
                            "aggregateTradeId": aggregate_trade_id,
                            "price": partial_price,
                            "realizedFraction": runner.realized_fraction,
                        }),
                    },
                    VirtualTradeEvent {
                        event_id: Uuid::now_v7(),
                        virtual_trade_id: runner.virtual_trade_id,
                        sequence: runner_sequence,
                        kind: VirtualTradeEventKind::RunnerActivated,
                        event_time: trade_time,
                        payload: json!({
                            "activeStop": runner.active_stop,
                            "remainingFraction": runner.remaining_fraction,
                        }),
                    },
                ]);
                transition.plan_feedback.extend(partial_feedback);
                transition.monitoring_events.push(monitoring_event(
                    "VIRTUAL_TRADE_PARTIAL_TARGET_FILLED",
                    "virtual_trade",
                    Some(runner.virtual_trade_id),
                    virtual_trade_payload(&runner),
                ));

                let terminal = exit_level(&runner, price);
                let final_trade = if let Some((exit_price, kind, reason)) = terminal {
                    let closed = close_trade(runner, trade_time, exit_price, reason);
                    transition.trade_events.push(VirtualTradeEvent {
                        event_id: Uuid::now_v7(),
                        virtual_trade_id: closed.virtual_trade_id,
                        sequence: self.next_sequence(closed.virtual_trade_id),
                        kind,
                        event_time: trade_time,
                        payload: terminal_trade_payload(aggregate_trade_id, price, exit_price),
                    });
                    transition
                        .plan_feedback
                        .extend(self.close_feedback(&closed, trade_time, reason));
                    transition.monitoring_events.push(monitoring_event(
                        "VIRTUAL_TRADE_CLOSED",
                        "virtual_trade",
                        Some(closed.virtual_trade_id),
                        virtual_trade_payload(&closed),
                    ));
                    closed
                } else {
                    runner
                };
                self.trades.insert(key, final_trade.clone());
                let state = if final_trade.trade_state.is_terminal() {
                    closed_tracker_state(
                        &final_trade,
                        final_trade.exit_reason.as_deref().unwrap_or("trade closed"),
                    )
                } else {
                    tracker_state_for_trade(&final_trade)
                };
                if first_tracker_state {
                    transition.tracker_state = Some(state);
                    first_tracker_state = false;
                } else {
                    transition.tracker_states.push(state);
                }
                let write = VirtualTradeWrite { trade: final_trade };
                if first_trade_write {
                    transition.virtual_trade_write = Some(write);
                    first_trade_write = false;
                } else {
                    transition.virtual_trade_writes.push(write);
                }
            }
            for (key, active, (exit_price, kind, reason)) in terminal_trades {
                let closed = close_trade(active, trade_time, exit_price, reason);
                self.trades.insert(key, closed.clone());
                let sequence = self.next_sequence(closed.virtual_trade_id);
                let feedback = self.close_feedback(&closed, trade_time, reason);
                let state = TrackerState {
                    signal_id: closed.signal_id,
                    symbol: closed.symbol.clone(),
                    state: json!({
                        "signalId": closed.signal_id,
                        "virtualTradeId": closed.virtual_trade_id,
                        "tradeState": closed.trade_state,
                        "exitReason": reason
                    }),
                    tracking_health: TrackingHealth::Live,
                };
                if first_tracker_state {
                    transition.tracker_state = Some(state);
                    first_tracker_state = false;
                } else {
                    transition.tracker_states.push(state);
                }
                let write = VirtualTradeWrite {
                    trade: closed.clone(),
                };
                if first_trade_write {
                    transition.virtual_trade_write = Some(write);
                    first_trade_write = false;
                } else {
                    transition.virtual_trade_writes.push(write);
                }
                transition.trade_events.push(VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id: closed.virtual_trade_id,
                    sequence,
                    kind,
                    event_time: trade_time,
                    payload: terminal_trade_payload(aggregate_trade_id, price, exit_price),
                });
                transition.plan_feedback.extend(feedback);
                transition.monitoring_events.push(monitoring_event(
                    "VIRTUAL_TRADE_CLOSED",
                    "virtual_trade",
                    Some(closed.virtual_trade_id),
                    virtual_trade_payload(&closed),
                ));
            }
            for (key, signal, recommendation_exit) in terminal_recommendations {
                self.recommendations.remove(&key);
                let (setup_state, event_kind, event_type, reason) = match recommendation_exit {
                    RecommendationExit::Expired => (
                        "EXPIRED",
                        SignalEventKind::Expired,
                        "RECOMMENDATION_EXPIRED",
                        "entry expired",
                    ),
                    RecommendationExit::Invalidated(reason) => (
                        "INVALIDATED",
                        SignalEventKind::Invalidated,
                        "RECOMMENDATION_INVALIDATED",
                        reason,
                    ),
                };
                let state = TrackerState {
                    signal_id: signal.signal_id,
                    symbol: signal.symbol.clone(),
                    state: json!({
                        "signalId": signal.signal_id,
                        "setupState": setup_state,
                        "tradeState": TradeState::NotEntered,
                        "reason": reason
                    }),
                    tracking_health: TrackingHealth::Live,
                };
                if first_tracker_state {
                    transition.tracker_state = Some(state);
                    first_tracker_state = false;
                } else {
                    transition.tracker_states.push(state);
                }
                transition.signal_events.push(SignalEvent {
                    event_id: Uuid::now_v7(),
                    signal_id: signal.signal_id,
                    sequence: 1,
                    kind: event_kind,
                    effective_at: trade_time,
                    recorded_at: event_time,
                    virtual_trade_id: None,
                    price: Some(price),
                    reason: Some(reason.to_owned()),
                });
                transition.monitoring_events.push(monitoring_event(
                    event_type,
                    "signal",
                    Some(signal.signal_id),
                    recommendation_terminal_payload(
                        &signal,
                        setup_state,
                        reason,
                        Some(price),
                        None,
                    ),
                ));
            }
            return Ok(transition);
        }
        Ok(DurableTransition::default())
    }

    fn apply_replay_candle(
        &mut self,
        candle: Candle,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        let close_time = candle.close_time;
        let transition = self.apply_ohlc_reconciliation(candle, true)?;
        for through in self.replaying.values_mut() {
            *through = (*through).max(close_time);
        }
        Ok(transition)
    }

    fn apply_ohlc_reconciliation(
        &mut self,
        candle: Candle,
        replay: bool,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        let mut excursion_checkpoints = Vec::new();
        for active in self.trades.values_mut().filter(|active| {
            active.symbol == candle.symbol
                && active.trade_state == TradeState::Open
                && candle.close_time >= active.actual_entry_time
                && candle_reaches_trade(&self.replaying, active, &candle, replay)
        }) {
            if update_ohlc_excursion(active, &candle) {
                excursion_checkpoints.push(active.clone());
            }
        }
        let partial_trades = self
            .trades
            .iter()
            .filter_map(|(key, active)| {
                if active.symbol != candle.symbol
                    || active.trade_state != TradeState::Open
                    || candle.close_time < active.actual_entry_time
                    || !candle_reaches_trade(&self.replaying, active, &candle, replay)
                {
                    return None;
                }
                let decision = ohlc_trade_decision(active, &candle, false);
                (decision.actions.first() == Some(&ExecutionKind::PartialTarget))
                    .then(|| (key.clone(), active.clone(), decision))
            })
            .collect::<Vec<_>>();
        let terminal_trades = self
            .trades
            .iter()
            .filter_map(|(key, active)| {
                if active.symbol != candle.symbol
                    || active.trade_state != TradeState::Open
                    || !candle_reaches_trade(&self.replaying, active, &candle, replay)
                {
                    return None;
                }
                if candle.close_time < active.actual_entry_time {
                    return None;
                }
                if ohlc_trade_decision(active, &candle, false).actions.first()
                    == Some(&ExecutionKind::PartialTarget)
                {
                    return None;
                }
                let include_entry = candle.open_time <= active.actual_entry_time;
                ohlc_exit_level(active, &candle, include_entry)
                    .map(|exit| (key.clone(), active.clone(), exit))
            })
            .collect::<Vec<_>>();
        let terminal_recommendations = self
            .recommendations
            .iter()
            .filter_map(|(key, signal)| {
                if replay
                    || signal.symbol != candle.symbol
                    || self.entry_boundary(signal) > candle.close_time
                    || self.market_closed(candle.close_time)
                {
                    return None;
                }
                recommendation_ohlc_exit(signal, &candle)
                    .map(|(exit, price)| (key.clone(), signal.clone(), exit, price))
            })
            .collect::<Vec<_>>();
        if partial_trades.is_empty()
            && terminal_trades.is_empty()
            && terminal_recommendations.is_empty()
        {
            let mut transition = DurableTransition::default();
            for checkpoint in excursion_checkpoints {
                append_virtual_trade_write(&mut transition, checkpoint);
            }
            return Ok(transition);
        }

        let mut transition = DurableTransition::default();
        let handled_trade_ids = terminal_trades
            .iter()
            .map(|(_, trade, _)| trade.virtual_trade_id)
            .chain(
                partial_trades
                    .iter()
                    .map(|(_, trade, _)| trade.virtual_trade_id),
            )
            .collect::<HashSet<_>>();
        let mut first_trade_write = true;
        let mut first_tracker_state = true;
        for (key, active, decision) in partial_trades {
            let mut runner = activate_runner(active, candle.close_time)?;
            runner.execution_model = ExecutionModel::OhlcConservative;
            runner.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
            let partial_price = runner.tp1_price.expect("FULL_POSITION trade must have TP1");
            let partial_feedback = self.partial_feedback(&runner, candle.close_time, partial_price);
            transition.trade_events.extend([
                VirtualTradeEvent {
                    event_id: reconciliation_event_id(
                        runner.signal_id,
                        runner.virtual_trade_id,
                        candle.close_time,
                        VirtualTradeEventKind::PartialTargetTouched.as_str(),
                    ),
                    virtual_trade_id: runner.virtual_trade_id,
                    sequence: self.next_sequence(runner.virtual_trade_id),
                    kind: VirtualTradeEventKind::PartialTargetTouched,
                    event_time: candle.close_time,
                    payload: json!({
                        "source": "OHLC_RECONCILED",
                        "price": partial_price,
                        "realizedFraction": runner.realized_fraction,
                    }),
                },
                VirtualTradeEvent {
                    event_id: reconciliation_event_id(
                        runner.signal_id,
                        runner.virtual_trade_id,
                        candle.close_time,
                        VirtualTradeEventKind::RunnerActivated.as_str(),
                    ),
                    virtual_trade_id: runner.virtual_trade_id,
                    sequence: self.next_sequence(runner.virtual_trade_id),
                    kind: VirtualTradeEventKind::RunnerActivated,
                    event_time: candle.close_time,
                    payload: json!({
                        "activeStop": runner.active_stop,
                        "remainingFraction": runner.remaining_fraction,
                    }),
                },
            ]);
            transition.plan_feedback.extend(partial_feedback);
            transition.monitoring_events.push(monitoring_event(
                "VIRTUAL_TRADE_PARTIAL_TARGET_FILLED",
                "virtual_trade",
                Some(runner.virtual_trade_id),
                virtual_trade_payload(&runner),
            ));

            let terminal = if decision.actions.contains(&ExecutionKind::TrailingStop) {
                Some(OhlcExit {
                    price: runner.active_stop,
                    kind: VirtualTradeEventKind::StopTouched,
                    reason: "trailing stop touched",
                    ambiguity: decision.ambiguity,
                })
            } else if decision.actions.contains(&ExecutionKind::Target) {
                Some(OhlcExit {
                    price: active_tp2_edge(&runner),
                    kind: VirtualTradeEventKind::TargetTouched,
                    reason: "target touched",
                    ambiguity: decision.ambiguity,
                })
            } else {
                None
            };
            let final_trade = if let Some(exit) = terminal {
                let mut closed = close_trade(runner, candle.close_time, exit.price, exit.reason);
                closed.execution_model = ExecutionModel::OhlcConservative;
                closed.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
                closed.ambiguity = exit.ambiguity;
                transition.trade_events.push(VirtualTradeEvent {
                    event_id: reconciliation_event_id(
                        closed.signal_id,
                        closed.virtual_trade_id,
                        candle.close_time,
                        exit.kind.as_str(),
                    ),
                    virtual_trade_id: closed.virtual_trade_id,
                    sequence: self.next_sequence(closed.virtual_trade_id),
                    kind: exit.kind,
                    event_time: candle.close_time,
                    payload: ohlc_terminal_payload(&candle, exit.price),
                });
                transition.plan_feedback.extend(self.close_feedback(
                    &closed,
                    candle.close_time,
                    exit.reason,
                ));
                transition.monitoring_events.push(monitoring_event(
                    "VIRTUAL_TRADE_CLOSED",
                    "virtual_trade",
                    Some(closed.virtual_trade_id),
                    virtual_trade_payload(&closed),
                ));
                closed
            } else {
                runner
            };
            self.trades.insert(key, final_trade.clone());
            let state = if final_trade.trade_state.is_terminal() {
                closed_tracker_state(
                    &final_trade,
                    final_trade.exit_reason.as_deref().unwrap_or("trade closed"),
                )
            } else {
                tracker_state_for_trade(&final_trade)
            };
            if first_tracker_state {
                transition.tracker_state = Some(state);
                first_tracker_state = false;
            } else {
                transition.tracker_states.push(state);
            }
            let write = VirtualTradeWrite { trade: final_trade };
            if first_trade_write {
                transition.virtual_trade_write = Some(write);
                first_trade_write = false;
            } else {
                transition.virtual_trade_writes.push(write);
            }
        }
        for (key, active, exit) in terminal_trades {
            let mut closed = close_trade(active, candle.close_time, exit.price, exit.reason);
            closed.execution_model = ExecutionModel::OhlcConservative;
            closed.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
            closed.ambiguity = exit.ambiguity;
            self.trades.insert(key, closed.clone());
            let sequence = self.next_sequence(closed.virtual_trade_id);
            transition.plan_feedback.extend(self.close_feedback(
                &closed,
                candle.close_time,
                exit.reason,
            ));
            let state = closed_tracker_state(&closed, exit.reason);
            if first_tracker_state {
                transition.tracker_state = Some(state);
                first_tracker_state = false;
            } else {
                transition.tracker_states.push(state);
            }
            let write = VirtualTradeWrite {
                trade: closed.clone(),
            };
            if first_trade_write {
                transition.virtual_trade_write = Some(write);
                first_trade_write = false;
            } else {
                transition.virtual_trade_writes.push(write);
            }
            transition.trade_events.push(VirtualTradeEvent {
                event_id: reconciliation_event_id(
                    closed.signal_id,
                    closed.virtual_trade_id,
                    candle.close_time,
                    exit.kind.as_str(),
                ),
                virtual_trade_id: closed.virtual_trade_id,
                sequence,
                kind: exit.kind,
                event_time: candle.close_time,
                payload: ohlc_terminal_payload(&candle, exit.price),
            });
            transition.monitoring_events.push(monitoring_event(
                "VIRTUAL_TRADE_CLOSED",
                "virtual_trade",
                Some(closed.virtual_trade_id),
                virtual_trade_payload(&closed),
            ));
        }
        for (key, signal, recommendation_exit, event_price) in terminal_recommendations {
            self.recommendations.remove(&key);
            let (setup_state, event_kind, event_type, reason) = match recommendation_exit {
                RecommendationExit::Expired => (
                    "EXPIRED",
                    SignalEventKind::Expired,
                    "RECOMMENDATION_EXPIRED",
                    "entry expired",
                ),
                RecommendationExit::Invalidated(reason) => (
                    "INVALIDATED",
                    SignalEventKind::Invalidated,
                    "RECOMMENDATION_INVALIDATED",
                    reason,
                ),
            };
            let state = TrackerState {
                signal_id: signal.signal_id,
                symbol: signal.symbol.clone(),
                state: json!({
                    "signalId": signal.signal_id,
                    "setupState": setup_state,
                    "tradeState": TradeState::NotEntered,
                    "reason": reason
                }),
                tracking_health: TrackingHealth::Live,
            };
            if first_tracker_state {
                transition.tracker_state = Some(state);
                first_tracker_state = false;
            } else {
                transition.tracker_states.push(state);
            }
            transition.signal_events.push(SignalEvent {
                event_id: Uuid::now_v7(),
                signal_id: signal.signal_id,
                sequence: 1,
                kind: event_kind,
                effective_at: candle.close_time,
                recorded_at: candle.close_time,
                virtual_trade_id: None,
                price: Some(event_price),
                reason: Some(reason.to_owned()),
            });
            transition.monitoring_events.push(monitoring_event(
                event_type,
                "signal",
                Some(signal.signal_id),
                recommendation_terminal_payload(
                    &signal,
                    setup_state,
                    reason,
                    Some(event_price),
                    None,
                ),
            ));
        }
        for checkpoint in excursion_checkpoints {
            if !handled_trade_ids.contains(&checkpoint.virtual_trade_id) {
                append_virtual_trade_write(&mut transition, checkpoint);
            }
        }
        Ok(transition)
    }

    fn apply_directive(
        &mut self,
        directive: ManagementDirective,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        if matches!(
            directive.kind,
            DirectiveKind::CancelEntry | DirectiveKind::ExpireEntry
        ) && let Some((key, signal)) = self
            .recommendations
            .iter()
            .find(|(_, signal)| signal.signal_id == directive.signal_id)
            .map(|(key, signal)| (key.clone(), signal.clone()))
        {
            let exit = if directive.kind == DirectiveKind::ExpireEntry {
                RecommendationExit::Expired
            } else {
                RecommendationExit::Invalidated("plan directive cancelled entry")
            };
            let event_price = directive
                .price
                .or_else(|| signal_entry_reference(&signal))
                .unwrap_or(Decimal::ONE);
            let mut transition = self.close_recommendation_before_entry(
                key,
                signal,
                directive.effective_at,
                event_price,
                exit,
            );
            transition
                .consumed_directive_ids
                .push(directive.directive_id);
            return Ok(transition);
        }
        let Some((key, existing)) = self
            .trades
            .iter()
            .find(|(_, trade)| {
                directive.virtual_trade_id == Some(trade.virtual_trade_id)
                    || trade.signal_id == directive.signal_id
            })
            .map(|(key, trade)| (key.clone(), trade.clone()))
        else {
            return Err(TrackerRuntimeError::UnknownDirective);
        };
        if existing.trade_state.is_terminal() {
            return Ok(DurableTransition::default());
        }
        if directive.kind == DirectiveKind::MoveStop {
            let Some(price) = directive.price else {
                return Err(TrackerRuntimeError::InvalidSignal(
                    "MOVE_STOP requires a price".to_owned(),
                ));
            };
            if !directive_matches_trade_version_and_order(&existing, &directive) {
                return Ok(ignored_directive_transition(
                    &existing,
                    &directive,
                    "MOVE_STOP is stale or belongs to another strategy version",
                ));
            }
            if !valid_stop_move(&existing, price) {
                return Ok(ignored_directive_transition(
                    &existing,
                    &directive,
                    "MOVE_STOP would loosen the active stop",
                ));
            }
            let mut moved = existing.clone();
            moved.active_stop = price;
            moved.last_management_effective_at = Some(directive.effective_at);
            moved.last_management_candle_time = Some(directive.source_closed_candle_time);
            self.trades.insert(key, moved.clone());
            let sequence = self.next_sequence(moved.virtual_trade_id);
            return Ok(DurableTransition {
                virtual_trade_write: Some(VirtualTradeWrite {
                    trade: moved.clone(),
                }),
                trade_events: vec![VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id: moved.virtual_trade_id,
                    sequence,
                    kind: VirtualTradeEventKind::StopMoved,
                    event_time: directive.effective_at,
                    payload: json!({ "activeStop": price, "reason": directive.reason }),
                }],
                consumed_directive_ids: vec![directive.directive_id],
                monitoring_events: vec![monitoring_event(
                    "VIRTUAL_TRADE_STOP_MOVED",
                    "virtual_trade",
                    Some(moved.virtual_trade_id),
                    virtual_trade_payload(&moved),
                )],
                ..DurableTransition::default()
            });
        }
        if directive.kind == DirectiveKind::MoveTarget {
            let Some(price) = directive.price else {
                return Err(TrackerRuntimeError::InvalidSignal(
                    "MOVE_TARGET requires a price".to_owned(),
                ));
            };
            if !valid_target_move(&existing, price) {
                return Err(TrackerRuntimeError::InvalidSignal(
                    "MOVE_TARGET must stay beyond the entry in trade direction".to_owned(),
                ));
            }
            let mut moved = existing.clone();
            if let (Some(low), Some(high)) = (moved.tp2_zone_low, moved.tp2_zone_high) {
                let width = high - low;
                match moved.direction {
                    Direction::Buy => {
                        moved.tp2_zone_low = Some(price);
                        moved.tp2_zone_high = Some(price + width);
                    }
                    Direction::Sell => {
                        moved.tp2_zone_low = Some(price - width);
                        moved.tp2_zone_high = Some(price);
                    }
                    Direction::None => {}
                }
                // Keep analytical primary_target stable when TP2 zone is present;
                // execution/display edges come from active_tp2_edge / zone bounds.
            } else {
                moved.primary_target = price;
            }
            self.trades.insert(key, moved.clone());
            let sequence = self.next_sequence(moved.virtual_trade_id);
            return Ok(DurableTransition {
                virtual_trade_write: Some(VirtualTradeWrite {
                    trade: moved.clone(),
                }),
                trade_events: vec![VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id: moved.virtual_trade_id,
                    sequence,
                    kind: VirtualTradeEventKind::TargetMoved,
                    event_time: directive.effective_at,
                    payload: json!({
                        "primaryTarget": decimal_text(moved.primary_target),
                        "tp2ExecutionEdge": decimal_text(active_tp2_edge(&moved)),
                        "tp2ZoneLow": moved.tp2_zone_low.map(decimal_text),
                        "tp2ZoneHigh": moved.tp2_zone_high.map(decimal_text),
                        "reason": directive.reason,
                    }),
                }],
                consumed_directive_ids: vec![directive.directive_id],
                monitoring_events: vec![monitoring_event(
                    "VIRTUAL_TRADE_TARGET_MOVED",
                    "virtual_trade",
                    Some(moved.virtual_trade_id),
                    virtual_trade_payload(&moved),
                )],
                ..DurableTransition::default()
            });
        }
        if !matches!(
            directive.kind,
            DirectiveKind::ExitAtNextOpen
                | DirectiveKind::ReverseAtNextOpen
                | DirectiveKind::SuggestClose
                | DirectiveKind::EmergencyExit
                | DirectiveKind::CancelEntry
                | DirectiveKind::ExpireEntry
        ) {
            return Err(TrackerRuntimeError::UnknownDirective);
        }
        if matches!(
            directive.kind,
            DirectiveKind::SuggestClose | DirectiveKind::EmergencyExit
        ) && directive.price.is_none()
        {
            return Err(TrackerRuntimeError::InvalidSignal(
                "close advice directive requires a price".to_owned(),
            ));
        }
        let closed = if let Some(price) = directive.price {
            close_trade(
                existing.clone(),
                directive.effective_at,
                price,
                &directive.reason,
            )
        } else {
            let mut closed = existing.clone();
            closed.trade_state = TradeState::Closed;
            closed.exit_time = Some(directive.effective_at);
            closed.duration_ms = Some(directive.effective_at - existing.actual_entry_time);
            closed.exit_reason = Some(directive.reason.clone());
            closed
        };
        self.trades.insert(key, closed.clone());
        let sequence = self.next_sequence(closed.virtual_trade_id);
        let feedback = self.close_feedback(&closed, directive.effective_at, &directive.reason);
        Ok(DurableTransition {
            tracker_state: Some(TrackerState {
                signal_id: closed.signal_id,
                symbol: closed.symbol.clone(),
                state: json!({
                    "signalId": closed.signal_id,
                    "virtualTradeId": closed.virtual_trade_id,
                    "tradeState": closed.trade_state,
                    "exitReason": directive.reason
                }),
                tracking_health: TrackingHealth::Live,
            }),
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: closed.clone(),
            }),
            trade_events: vec![VirtualTradeEvent {
                event_id: Uuid::now_v7(),
                virtual_trade_id: closed.virtual_trade_id,
                sequence,
                kind: VirtualTradeEventKind::Closed,
                event_time: directive.effective_at,
                payload: json!({ "reason": directive.reason, "price": directive.price }),
            }],
            consumed_directive_ids: vec![directive.directive_id],
            plan_feedback: feedback.into_iter().collect(),
            monitoring_events: vec![monitoring_event(
                "VIRTUAL_TRADE_CLOSED",
                "virtual_trade",
                Some(closed.virtual_trade_id),
                close_directive_payload(&closed, directive.kind),
            )],
            ..DurableTransition::default()
        })
    }

    fn apply_canonical_open(
        &mut self,
        candle: Candle,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        let Some((key, signal)) = self
            .recommendations
            .iter()
            .find(|(_, signal)| {
                signal.symbol == candle.symbol
                    && signal.target_timeframe == candle.timeframe
                    && signal.execution_instruction == ExecutionInstruction::NextOpen
                    && self.entry_boundary(signal) <= candle.open_time
            })
            .map(|(key, signal)| (key.clone(), signal.clone()))
        else {
            return Ok(DurableTransition::default());
        };
        if let Some(exit) = recommendation_exit(&signal, candle.open, candle.open_time) {
            return Ok(self.close_recommendation_before_entry(
                key,
                signal,
                candle.open_time,
                candle.open,
                exit,
            ));
        }
        let pending = PendingNextOpen {
            expected_open_time: self.entry_boundary(&signal),
            signal: signal.clone(),
            buffered_trades: Vec::new(),
        };
        match pending.decide(&candle, None) {
            NextOpenDecision::Enter { time, price } => {
                let virtual_trade_id =
                    reconciliation_trade_id(signal.signal_id, time, "CANONICAL_OPEN");
                let opened = match virtual_trade_from_entry(
                    &signal,
                    virtual_trade_id,
                    time,
                    price,
                    Some(ReconciliationSource::OhlcReconciled),
                ) {
                    Ok(trade) => trade,
                    Err(TrackerRuntimeError::InvalidSignal(reason))
                        if reason == EXECUTION_PLAN_RR_FAILED =>
                    {
                        return Ok(self.close_recommendation_before_entry(
                            key,
                            signal,
                            time,
                            price,
                            RecommendationExit::Invalidated(EXECUTION_PLAN_RR_FAILED),
                        ));
                    }
                    Err(error) => return Err(error),
                };
                self.recommendations.remove(&key);
                let mut stored = opened.clone();
                let mut tracker = tracker_state(&signal, TrackingHealth::Live, Some(&opened));
                let mut trade_events = vec![VirtualTradeEvent {
                    event_id: reconciliation_event_id(
                        signal.signal_id,
                        virtual_trade_id,
                        time,
                        VirtualTradeEventKind::EntryTriggered.as_str(),
                    ),
                    virtual_trade_id,
                    sequence: 1,
                    kind: VirtualTradeEventKind::EntryTriggered,
                    event_time: time,
                    payload: json!({
                        "openTime": candle.open_time,
                        "price": price
                    }),
                }];
                let mut feedback = self.entry_feedback(&opened).into_iter().collect::<Vec<_>>();
                let mut monitoring_events = vec![monitoring_event(
                    "VIRTUAL_TRADE_OPENED",
                    "virtual_trade",
                    Some(virtual_trade_id),
                    virtual_trade_payload(&opened),
                )];
                let mut next_sequence = 2;
                let mut partial_filled = false;
                if !ohlc_stop_touched(&opened, &candle)
                    && ohlc_partial_target_touched(&opened, &candle)
                {
                    let mut runner = activate_runner(opened.clone(), candle.close_time)?;
                    runner.execution_model = ExecutionModel::OhlcConservative;
                    runner.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
                    let partial_price =
                        runner.tp1_price.expect("FULL_POSITION trade must have TP1");
                    trade_events.extend([
                        VirtualTradeEvent {
                            event_id: reconciliation_event_id(
                                signal.signal_id,
                                virtual_trade_id,
                                candle.close_time,
                                VirtualTradeEventKind::PartialTargetTouched.as_str(),
                            ),
                            virtual_trade_id,
                            sequence: next_sequence,
                            kind: VirtualTradeEventKind::PartialTargetTouched,
                            event_time: candle.close_time,
                            payload: json!({
                                "source": "OHLC_RECONCILED",
                                "price": partial_price,
                                "realizedFraction": runner.realized_fraction,
                            }),
                        },
                        VirtualTradeEvent {
                            event_id: reconciliation_event_id(
                                signal.signal_id,
                                virtual_trade_id,
                                candle.close_time,
                                VirtualTradeEventKind::RunnerActivated.as_str(),
                            ),
                            virtual_trade_id,
                            sequence: next_sequence + 1,
                            kind: VirtualTradeEventKind::RunnerActivated,
                            event_time: candle.close_time,
                            payload: json!({
                                "activeStop": runner.active_stop,
                                "remainingFraction": runner.remaining_fraction,
                            }),
                        },
                    ]);
                    next_sequence += 2;
                    feedback.extend(self.partial_feedback(
                        &runner,
                        candle.close_time,
                        partial_price,
                    ));
                    monitoring_events.push(monitoring_event(
                        "VIRTUAL_TRADE_PARTIAL_TARGET_FILLED",
                        "virtual_trade",
                        Some(virtual_trade_id),
                        virtual_trade_payload(&runner),
                    ));
                    stored = runner;
                    partial_filled = true;
                }
                let entry_candle_exit = if partial_filled {
                    ohlc_entry_candle_runner_exit(&stored, &candle)
                } else {
                    ohlc_exit_level(&stored, &candle, true)
                };
                if let Some(exit) = entry_candle_exit {
                    let mut closed =
                        close_trade(stored.clone(), candle.close_time, exit.price, exit.reason);
                    closed.execution_model = ExecutionModel::OhlcConservative;
                    closed.reconciliation_source = Some(ReconciliationSource::OhlcReconciled);
                    closed.ambiguity = exit.ambiguity;
                    trade_events.push(VirtualTradeEvent {
                        event_id: reconciliation_event_id(
                            signal.signal_id,
                            virtual_trade_id,
                            candle.close_time,
                            exit.kind.as_str(),
                        ),
                        virtual_trade_id,
                        sequence: next_sequence,
                        kind: exit.kind,
                        event_time: candle.close_time,
                        payload: ohlc_terminal_payload(&candle, exit.price),
                    });
                    feedback.extend(self.close_feedback(&closed, candle.close_time, exit.reason));
                    tracker = closed_tracker_state(&closed, exit.reason);
                    let closed_event = monitoring_event(
                        "VIRTUAL_TRADE_CLOSED",
                        "virtual_trade",
                        Some(virtual_trade_id),
                        virtual_trade_payload(&closed),
                    );
                    if partial_filled {
                        monitoring_events.remove(0);
                        monitoring_events.push(closed_event);
                    } else {
                        monitoring_events = vec![closed_event];
                    }
                    stored = closed;
                    self.next_trade_sequence
                        .insert(virtual_trade_id, next_sequence + 1);
                } else {
                    self.next_trade_sequence
                        .insert(virtual_trade_id, next_sequence);
                }
                self.trades.insert(key, stored.clone());
                Ok(DurableTransition {
                    tracker_state: Some(tracker),
                    virtual_trade_write: Some(VirtualTradeWrite { trade: stored }),
                    trade_events,
                    signal_events: vec![SignalEvent {
                        event_id: reconciliation_event_id(
                            signal.signal_id,
                            virtual_trade_id,
                            time,
                            SignalEventKind::Triggered.as_str(),
                        ),
                        signal_id: signal.signal_id,
                        sequence: 1,
                        kind: SignalEventKind::Triggered,
                        effective_at: time,
                        recorded_at: candle.close_time,
                        virtual_trade_id: Some(virtual_trade_id),
                        price: Some(price),
                        reason: Some("next open entered".to_owned()),
                    }],
                    plan_feedback: feedback,
                    monitoring_events,
                    ..DurableTransition::default()
                })
            }
            NextOpenDecision::Cancel { reason } => Ok(self.close_recommendation_before_entry(
                key,
                signal,
                candle.open_time,
                candle.open,
                RecommendationExit::Invalidated(match reason {
                    crate::PreEntryCancellation::TargetCrossed => "target touched before entry",
                    crate::PreEntryCancellation::StopCrossed => "stop touched before entry",
                    crate::PreEntryCancellation::InsufficientRiskReward => {
                        "insufficient risk reward"
                    }
                    crate::PreEntryCancellation::MissingValidation => {
                        "missing pre-entry validation"
                    }
                    crate::PreEntryCancellation::WrongPlan => "wrong pre-entry validation plan",
                }),
            )),
            NextOpenDecision::Reconcile { .. } => Ok(self.close_recommendation_before_entry(
                key,
                signal,
                candle.open_time,
                candle.open,
                RecommendationExit::Invalidated("canonical open skipped expected entry"),
            )),
            NextOpenDecision::Duplicate | NextOpenDecision::Wait => {
                Ok(DurableTransition::default())
            }
        }
    }

    fn close_recommendation_before_entry(
        &mut self,
        key: PlanProfileKey,
        signal: SignalDefinition,
        event_time: i64,
        event_price: Decimal,
        exit: RecommendationExit,
    ) -> DurableTransition {
        self.recommendations.remove(&key);
        let (setup_state, event_kind, event_type, reason) = match exit {
            RecommendationExit::Expired => (
                "EXPIRED",
                SignalEventKind::Expired,
                "RECOMMENDATION_EXPIRED",
                "entry expired",
            ),
            RecommendationExit::Invalidated(reason) => (
                "INVALIDATED",
                SignalEventKind::Invalidated,
                "RECOMMENDATION_INVALIDATED",
                reason,
            ),
        };
        DurableTransition {
            tracker_state: Some(TrackerState {
                signal_id: signal.signal_id,
                symbol: signal.symbol.clone(),
                state: json!({
                    "signalId": signal.signal_id,
                    "setupState": setup_state,
                    "tradeState": TradeState::NotEntered,
                    "reason": reason
                }),
                tracking_health: TrackingHealth::Live,
            }),
            signal_events: vec![SignalEvent {
                event_id: Uuid::now_v7(),
                signal_id: signal.signal_id,
                sequence: 1,
                kind: event_kind,
                effective_at: event_time,
                recorded_at: event_time,
                virtual_trade_id: None,
                price: Some(event_price),
                reason: Some(reason.to_owned()),
            }],
            monitoring_events: vec![monitoring_event(
                event_type,
                "signal",
                Some(signal.signal_id),
                recommendation_terminal_payload(
                    &signal,
                    setup_state,
                    reason,
                    Some(event_price),
                    None,
                ),
            )],
            ..DurableTransition::default()
        }
    }

    fn apply_reverse(
        &mut self,
        directive: ManagementDirective,
        reverse_signal: SignalDefinition,
        canonical_open: Candle,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        if reverse_signal.plan != analysis_domain::PlanKind::TrendBreakout
            || directive.kind != DirectiveKind::ReverseAtNextOpen
        {
            return Err(TrackerRuntimeError::UnknownDirective);
        }
        let Some((key, existing)) = self
            .trades
            .iter()
            .find(|(_, trade)| directive.virtual_trade_id == Some(trade.virtual_trade_id))
            .map(|(key, trade)| (key.clone(), trade.clone()))
        else {
            return Err(TrackerRuntimeError::UnknownDirective);
        };
        if existing.trade_state.is_terminal() {
            return Ok(DurableTransition::default());
        }
        let closed = close_trade(
            existing,
            canonical_open.open_time,
            canonical_open.open,
            "reverse at next open",
        );
        let reverse_trade_id = Uuid::now_v7();
        let entry_trade = AggregateTrade::new(
            reverse_signal.symbol.clone(),
            0,
            0,
            0,
            canonical_open.open_time,
            canonical_open.open_time,
            canonical_open.open,
        )
        .map_err(|error| TrackerRuntimeError::InvalidSignal(error.to_string()))?;
        let opened = virtual_trade_from_signal(&reverse_signal, reverse_trade_id, &entry_trade)?;
        self.trades.insert(key, opened.clone());
        self.next_trade_sequence.insert(reverse_trade_id, 2);
        if let Some(through) = self.replaying.remove(&closed.virtual_trade_id) {
            self.replaying.insert(reverse_trade_id, through);
        }
        let close_feedback = self.close_feedback(&closed, canonical_open.open_time, "reverse");
        let entry_feedback = self.entry_feedback(&opened);
        Ok(DurableTransition {
            tracker_state: Some(tracker_state(
                &reverse_signal,
                TrackingHealth::Live,
                Some(&opened),
            )),
            virtual_trade_write: Some(VirtualTradeWrite {
                trade: closed.clone(),
            }),
            virtual_trade_writes: vec![VirtualTradeWrite {
                trade: opened.clone(),
            }],
            trade_events: vec![
                VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id: closed.virtual_trade_id,
                    sequence: self.next_sequence(closed.virtual_trade_id),
                    kind: VirtualTradeEventKind::Reversed,
                    event_time: canonical_open.open_time,
                    payload: json!({ "price": canonical_open.open }),
                },
                VirtualTradeEvent {
                    event_id: Uuid::now_v7(),
                    virtual_trade_id: opened.virtual_trade_id,
                    sequence: 1,
                    kind: VirtualTradeEventKind::EntryTriggered,
                    event_time: canonical_open.open_time,
                    payload: json!({ "price": canonical_open.open }),
                },
            ],
            plan_feedback: close_feedback.into_iter().chain(entry_feedback).collect(),
            consumed_directive_ids: vec![directive.directive_id],
            monitoring_events: vec![monitoring_event(
                "VIRTUAL_TRADE_REVERSED",
                "virtual_trade",
                Some(opened.virtual_trade_id),
                extend_virtual_trade_payload(
                    &opened,
                    json!({ "closedTradeId": closed.virtual_trade_id }),
                ),
            )],
            ..DurableTransition::default()
        })
    }

    fn apply_signal_event(
        &mut self,
        event: SignalEvent,
    ) -> Result<DurableTransition, TrackerRuntimeError> {
        if matches!(
            event.kind,
            SignalEventKind::Invalidated | SignalEventKind::Expired
        ) {
            self.recommendations
                .retain(|_, signal| signal.signal_id != event.signal_id);
        }
        Ok(DurableTransition::default())
    }

    /// Per-trade bookkeeping is only needed while a trade still holds a slot; a
    /// replaced trade can never emit again, so its ids would otherwise accumulate
    /// for the life of the process and be cloned on every checkpoint.
    fn forget_replaced_trades(&mut self) {
        let held: HashSet<Uuid> = self
            .trades
            .values()
            .map(|trade| trade.virtual_trade_id)
            .collect();
        self.entry_feedback_sent.retain(|id| held.contains(id));
        self.partial_feedback_sent.retain(|id| held.contains(id));
        self.close_feedback_sent.retain(|id| held.contains(id));
        self.next_trade_sequence.retain(|id, _| held.contains(id));
        self.replaying.retain(|id, _| held.contains(id));
    }

    fn entry_feedback(&mut self, trade: &VirtualTrade) -> Option<PlanFeedback> {
        if !self.entry_feedback_sent.insert(trade.virtual_trade_id) {
            return None;
        }
        Some(PlanFeedback::ActualEntry {
            signal_id: trade.signal_id,
            virtual_trade_id: trade.virtual_trade_id,
            time: trade.actual_entry_time,
            price: trade.actual_entry_price,
        })
    }

    fn close_feedback(
        &mut self,
        trade: &VirtualTrade,
        time: i64,
        reason: &str,
    ) -> Option<PlanFeedback> {
        if !self.close_feedback_sent.insert(trade.virtual_trade_id) {
            return None;
        }
        Some(PlanFeedback::TradeClosed {
            signal_id: trade.signal_id,
            virtual_trade_id: trade.virtual_trade_id,
            time,
            reason: reason.to_owned(),
        })
    }

    fn partial_feedback(
        &mut self,
        trade: &VirtualTrade,
        time: i64,
        price: Decimal,
    ) -> Option<PlanFeedback> {
        if !self.partial_feedback_sent.insert(trade.virtual_trade_id) {
            return None;
        }
        Some(PlanFeedback::PartialTargetFilled {
            signal_id: trade.signal_id,
            virtual_trade_id: trade.virtual_trade_id,
            time,
            price,
            realized_fraction: trade.realized_fraction,
            remaining_fraction: trade.remaining_fraction,
        })
    }

    fn next_sequence(&mut self, virtual_trade_id: Uuid) -> i64 {
        let sequence = self
            .next_trade_sequence
            .entry(virtual_trade_id)
            .or_insert(1);
        let current = *sequence;
        *sequence += 1;
        current
    }
}

fn key_for_signal(signal: &SignalDefinition) -> PlanProfileKey {
    PlanProfileKey {
        symbol: signal.symbol.clone(),
        plan: signal.plan,
        target_timeframe: signal.target_timeframe,
    }
}

fn tracker_state(
    signal: &SignalDefinition,
    health: TrackingHealth,
    trade: Option<&VirtualTrade>,
) -> TrackerState {
    TrackerState {
        signal_id: signal.signal_id,
        symbol: signal.symbol.clone(),
        state: json!({
            "signalId": signal.signal_id,
            "setupState": "ENTRY_AVAILABLE",
            "tradeState": trade.map(|trade| trade.trade_state).unwrap_or(TradeState::NotEntered),
            "activeVirtualTradeId": trade.map(|trade| trade.virtual_trade_id)
        }),
        tracking_health: health,
    }
}

pub fn signal_recommendation_payload(signal: &SignalDefinition) -> serde_json::Value {
    let execution_plan = signal.execution_plan.as_ref();
    let tp2_execution_edge = execution_plan.map(|plan| match signal.direction {
        Direction::Buy => plan.tp2_zone.low,
        Direction::Sell => plan.tp2_zone.high,
        Direction::None => signal.primary_target,
    });
    json!({
        "signalId": signal.signal_id,
        "symbol": signal.symbol,
        "plan": signal.plan,
        "timeframe": signal.target_timeframe,
        "condition": "ENTRY_AVAILABLE",
        "direction": signal.direction,
        "execution": signal.execution_instruction,
        "entryLow": signal.entry_zone.as_ref().map(|zone| zone.low.to_string()),
        "entryHigh": signal.entry_zone.as_ref().map(|zone| zone.high.to_string()),
        "projectedEntry": signal.projected_entry.map(|price| price.to_string()),
        "stopLoss": signal.stop_loss.to_string(),
        "primaryTarget": signal.primary_target.to_string(),
        "analyticalTarget": decimal_text(signal.primary_target),
        "thesisInvalidation": execution_plan.map(|plan| decimal_text(plan.thesis_invalidation)),
        "hardStop": execution_plan.map(|plan| decimal_text(plan.hard_stop)),
        "tp1Price": Value::Null,
        "tp1R": execution_plan.map(|plan| decimal_text(plan.tp1.trigger_r)),
        "tp1Fraction": execution_plan.map(|plan| decimal_text(plan.tp1.fraction)),
        "tp1Filled": false,
        "tp1FilledAt": Value::Null,
        "tp2Zone": execution_plan.map(|plan| json!({
            "low": decimal_text(plan.tp2_zone.low),
            "high": decimal_text(plan.tp2_zone.high),
        })),
        "tp2ExecutionEdge": tp2_execution_edge.map(decimal_text),
        "referenceLevels": signal.reference_levels,
        "riskReward": signal.risk_reward.to_string(),
        "confidence": signal.confidence.to_string(),
        "htfContext": signal.htf_context.clone(),
        "effectiveAt": signal.effective_at,
        "expiresAt": signal.expires_at,
        "releaseId": signal.release_id,
        "algorithmVersion": signal.algorithm_version,
        "parameterVersion": signal.parameter_version,
    })
}

fn recommendation_terminal_payload(
    signal: &SignalDefinition,
    setup_state: &str,
    reason: &str,
    price: Option<Decimal>,
    replacement_signal_id: Option<Uuid>,
) -> serde_json::Value {
    let mut payload = signal_recommendation_payload(signal);
    payload["condition"] = json!(setup_state);
    payload["setupState"] = json!(setup_state);
    payload["tradeState"] = json!(TradeState::NotEntered);
    payload["reason"] = json!(reason);
    if let Some(price) = price {
        payload["price"] = json!(price);
    }
    if let Some(replacement_signal_id) = replacement_signal_id {
        payload["replacementSignalId"] = json!(replacement_signal_id);
    }
    payload
}

pub fn virtual_trade_payload(trade: &VirtualTrade) -> serde_json::Value {
    let has_tp2 = trade.tp2_zone_low.is_some() && trade.tp2_zone_high.is_some();
    let tp2_zone = match (trade.tp2_zone_low, trade.tp2_zone_high) {
        (Some(low), Some(high)) => Some(json!({
            "low": decimal_text(low),
            "high": decimal_text(high),
        })),
        _ => None,
    };
    let mut payload = json!({
        "virtualTradeId": trade.virtual_trade_id,
        "signalId": trade.signal_id,
        "symbol": trade.symbol,
        "plan": trade.plan,
        "timeframe": trade.timeframe,
        "interval": trade.timeframe,
        "direction": trade.direction,
        "tradeState": trade.trade_state,
        "positionStage": trade.position_stage,
        "tradeHealth": trade.trade_health,
        "actualEntryTime": trade.actual_entry_time,
        "actualEntryPrice": decimal_text(trade.actual_entry_price),
        "exitTime": trade.exit_time,
        "exitPrice": trade.exit_price.map(|value| value.to_string()),
        "primaryTarget": decimal_text(trade.primary_target),
        "referenceLevels": trade.reference_levels,
        "initialStop": decimal_text(trade.initial_stop),
        "activeStop": decimal_text(trade.active_stop),
        "thesisInvalidation": trade.thesis_invalidation.map(decimal_text),
        "tp1Price": trade.tp1_price.map(decimal_text),
        "tp1R": trade.tp1_r.map(decimal_text),
        "tp1Fraction": trade.tp1_fraction.map(decimal_text),
        "tp1FilledAt": trade.tp1_filled_at,
        "remainingFraction": decimal_text(trade.remaining_fraction),
        "realizedFraction": decimal_text(trade.realized_fraction),
        "realizedR": decimal_text(trade.realized_r),
        "tp2ZoneLow": trade.tp2_zone_low.map(decimal_text),
        "tp2ZoneHigh": trade.tp2_zone_high.map(decimal_text),
        "exitReason": trade.exit_reason,
        "grossR": trade.gross_r.map(|value| value.to_string()),
        "mfeR": trade.mfe_r.to_string(),
        "maeR": trade.mae_r.to_string(),
        "durationMs": trade.duration_ms,
        "releaseId": trade.versions.release_id,
        "algorithmVersion": trade.versions.algorithm_version,
        "parameterVersion": trade.versions.parameter_version,
    });
    payload["analyticalTarget"] = json!(decimal_text(trade.primary_target));
    payload["hardStop"] = json!(
        (trade.position_stage != PositionStage::Legacy).then(|| decimal_text(trade.initial_stop))
    );
    payload["tp1Filled"] = json!(trade.tp1_filled_at.is_some());
    payload["tp2Zone"] = json!(tp2_zone);
    payload["tp2ExecutionEdge"] = json!(has_tp2.then(|| decimal_text(active_tp2_edge(trade))));
    payload["executionModel"] = json!(trade.execution_model);
    payload["ambiguity"] = json!(trade.ambiguity);
    payload["reconciliationSource"] = json!(trade.reconciliation_source);
    payload
}

fn decimal_text(value: Decimal) -> String {
    value.normalize().to_string()
}

pub fn virtual_trade_payload_with_signal(
    trade: &VirtualTrade,
    signal: Option<&SignalDefinition>,
) -> serde_json::Value {
    let mut payload = virtual_trade_payload(trade);
    if let Some(context) = signal.and_then(|signal| signal.htf_context.clone()) {
        payload["htfContext"] = json!(context);
    }
    payload
}

fn virtual_trade_from_signal(
    signal: &SignalDefinition,
    virtual_trade_id: Uuid,
    trade: &AggregateTrade,
) -> Result<VirtualTrade, TrackerRuntimeError> {
    virtual_trade_from_entry(
        signal,
        virtual_trade_id,
        trade.trade_time,
        trade.price,
        Some(ReconciliationSource::ExactTrade),
    )
}

fn virtual_trade_from_entry(
    signal: &SignalDefinition,
    virtual_trade_id: Uuid,
    entry_time: i64,
    entry_price: Decimal,
    reconciliation_source: Option<ReconciliationSource>,
) -> Result<VirtualTrade, TrackerRuntimeError> {
    if !matches!(signal.direction, Direction::Buy | Direction::Sell) {
        return Err(TrackerRuntimeError::InvalidSignal(
            "direction must be BUY or SELL".to_owned(),
        ));
    }
    let (
        position_stage,
        thesis_invalidation,
        tp1_r,
        tp1_fraction,
        tp1_price,
        tp2_zone_low,
        tp2_zone_high,
        primary_target,
    ) = match signal.execution_plan.as_ref() {
        Some(plan) => {
            let initial_risk = (entry_price - plan.hard_stop).abs();
            if initial_risk <= Decimal::ZERO {
                return Err(TrackerRuntimeError::InvalidSignal(
                    EXECUTION_PLAN_RR_FAILED.to_owned(),
                ));
            }
            let tp1_price = match signal.direction {
                Direction::Buy => entry_price + initial_risk * plan.tp1.trigger_r,
                Direction::Sell => entry_price - initial_risk * plan.tp1.trigger_r,
                Direction::None => unreachable!("direction validated above"),
            };
            let crosses_tp2_edge = match signal.direction {
                Direction::Buy => tp1_price > plan.tp2_zone.low,
                Direction::Sell => tp1_price < plan.tp2_zone.high,
                Direction::None => unreachable!("direction validated above"),
            };
            if crosses_tp2_edge {
                return Err(TrackerRuntimeError::InvalidSignal(
                    EXECUTION_PLAN_RR_FAILED.to_owned(),
                ));
            }
            (
                PositionStage::FullPosition,
                Some(plan.thesis_invalidation),
                Some(plan.tp1.trigger_r),
                Some(plan.tp1.fraction),
                Some(tp1_price),
                Some(plan.tp2_zone.low),
                Some(plan.tp2_zone.high),
                signal.primary_target,
            )
        }
        None => (
            PositionStage::Legacy,
            None,
            None,
            None,
            None,
            None,
            None,
            signal.primary_target,
        ),
    };
    Ok(VirtualTrade {
        virtual_trade_id,
        signal_id: signal.signal_id,
        symbol: signal.symbol.clone(),
        plan: signal.plan,
        timeframe: signal.target_timeframe,
        direction: signal.direction,
        trade_state: TradeState::Open,
        position_stage,
        trade_health: TradeHealth::Healthy,
        actual_entry_time: entry_time,
        actual_entry_price: entry_price,
        exit_time: None,
        exit_price: None,
        primary_target,
        reference_levels: signal
            .reference_levels
            .iter()
            .map(|level| level.price)
            .collect(),
        initial_stop: signal.stop_loss,
        active_stop: signal.stop_loss,
        thesis_invalidation,
        tp1_price,
        tp1_r,
        tp1_fraction,
        tp1_filled_at: None,
        remaining_fraction: Decimal::ONE,
        realized_fraction: Decimal::ZERO,
        realized_r: Decimal::ZERO,
        tp2_zone_low,
        tp2_zone_high,
        last_management_effective_at: None,
        last_management_candle_time: None,
        versions: analysis_domain::VersionIdentity {
            release_id: signal.release_id,
            algorithm_version: signal.algorithm_version.clone(),
            parameter_version: signal.parameter_version.clone(),
        },
        execution_model: ExecutionModel::LiveTrade,
        reconciliation_source,
        ambiguity: Ambiguity::None,
        gross_r: None,
        mfe_r: Decimal::ZERO,
        mae_r: Decimal::ZERO,
        duration_ms: None,
        exit_reason: None,
    })
}

#[derive(Clone, Copy, Debug)]
struct OhlcExit {
    price: Decimal,
    kind: VirtualTradeEventKind,
    reason: &'static str,
    ambiguity: Ambiguity,
}

/// A replaying trade takes only replay candles it has not seen. Every other trade takes only live ones.
fn candle_reaches_trade(
    replaying: &HashMap<Uuid, i64>,
    trade: &VirtualTrade,
    candle: &Candle,
    replay: bool,
) -> bool {
    match replaying.get(&trade.virtual_trade_id) {
        Some(through) => replay && candle.close_time > *through,
        None => !replay,
    }
}

fn ohlc_exit_level(trade: &VirtualTrade, candle: &Candle, include_entry: bool) -> Option<OhlcExit> {
    if candle.close_time < trade.actual_entry_time {
        return None;
    }
    let trailing_stop = (trade.position_stage != PositionStage::FullPosition
        && trade.active_stop != trade.initial_stop)
        .then_some(trade.active_stop);
    let decision = ohlc_trade_decision(trade, candle, include_entry);
    match decision.outcome {
        AmbiguousOutcome::StopFirst
        | AmbiguousOutcome::PartialThenStop
        | AmbiguousOutcome::SingleKnown(ExecutionKind::Stop)
        | AmbiguousOutcome::SingleKnown(ExecutionKind::TrailingStop) => Some(OhlcExit {
            price: trade.active_stop,
            kind: VirtualTradeEventKind::StopTouched,
            reason: if trailing_stop.is_some() {
                "trailing stop touched"
            } else {
                "initial stop touched"
            },
            ambiguity: decision.ambiguity,
        }),
        AmbiguousOutcome::EntryThenTarget
        | AmbiguousOutcome::PartialThenTarget
        | AmbiguousOutcome::SingleKnown(ExecutionKind::Target) => Some(OhlcExit {
            price: active_tp2_edge(trade),
            kind: VirtualTradeEventKind::TargetTouched,
            reason: "target touched",
            ambiguity: decision.ambiguity,
        }),
        AmbiguousOutcome::SingleKnown(ExecutionKind::Entry)
        | AmbiguousOutcome::SingleKnown(ExecutionKind::PartialTarget)
        | AmbiguousOutcome::SingleKnown(ExecutionKind::NoExecution) => None,
    }
}

fn ohlc_trade_decision(
    trade: &VirtualTrade,
    candle: &Candle,
    include_entry: bool,
) -> AmbiguityDecision {
    let trailing_stop = (trade.position_stage != PositionStage::FullPosition
        && trade.active_stop != trade.initial_stop)
        .then_some(trade.active_stop);
    classify_ohlc_ambiguity(OhlcExecutionRange {
        direction: trade.direction,
        entry_price: trade.actual_entry_price,
        target: active_tp2_edge(trade),
        tp1: trade.tp1_price,
        tp2_edge: Some(active_tp2_edge(trade)),
        stop: trade.initial_stop,
        trailing_stop,
        position_stage: trade.position_stage,
        entry_low: include_entry.then_some(trade.actual_entry_price),
        entry_high: include_entry.then_some(trade.actual_entry_price),
        low: candle.low,
        high: candle.high,
    })
}

fn active_tp2_edge(trade: &VirtualTrade) -> Decimal {
    match trade.direction {
        Direction::Buy => trade.tp2_zone_low.unwrap_or(trade.primary_target),
        Direction::Sell => trade.tp2_zone_high.unwrap_or(trade.primary_target),
        Direction::None => trade.primary_target,
    }
}

fn ohlc_entry_candle_runner_exit(trade: &VirtualTrade, candle: &Candle) -> Option<OhlcExit> {
    let crossed_breakeven_after_open = match trade.direction {
        Direction::Buy => candle.low <= trade.active_stop,
        Direction::Sell => candle.high >= trade.active_stop,
        Direction::None => false,
    };
    if crossed_breakeven_after_open {
        return Some(OhlcExit {
            price: trade.active_stop,
            kind: VirtualTradeEventKind::StopTouched,
            reason: "trailing stop touched",
            ambiguity: Ambiguity::Ambiguous,
        });
    }

    let tp2_edge = active_tp2_edge(trade);
    let target_touched = match trade.direction {
        Direction::Buy => candle.high >= tp2_edge,
        Direction::Sell => candle.low <= tp2_edge,
        Direction::None => false,
    };
    target_touched.then_some(OhlcExit {
        price: tp2_edge,
        kind: VirtualTradeEventKind::TargetTouched,
        reason: "target touched",
        ambiguity: Ambiguity::None,
    })
}

fn closed_tracker_state(trade: &VirtualTrade, reason: &str) -> TrackerState {
    TrackerState {
        signal_id: trade.signal_id,
        symbol: trade.symbol.clone(),
        state: json!({
            "signalId": trade.signal_id,
            "virtualTradeId": trade.virtual_trade_id,
            "tradeState": trade.trade_state,
            "exitReason": reason
        }),
        tracking_health: TrackingHealth::Live,
    }
}

fn tracker_state_for_trade(trade: &VirtualTrade) -> TrackerState {
    TrackerState {
        signal_id: trade.signal_id,
        symbol: trade.symbol.clone(),
        state: json!({
            "signalId": trade.signal_id,
            "virtualTradeId": trade.virtual_trade_id,
            "tradeState": trade.trade_state,
            "positionStage": trade.position_stage,
            "remainingFraction": decimal_text(trade.remaining_fraction),
            "realizedFraction": decimal_text(trade.realized_fraction),
            "realizedR": decimal_text(trade.realized_r),
        }),
        tracking_health: TrackingHealth::Live,
    }
}

fn ohlc_terminal_payload(candle: &Candle, exit_price: Decimal) -> serde_json::Value {
    json!({
        "source": "OHLC_RECONCILED",
        "openTime": candle.open_time,
        "closeTime": candle.close_time,
        "high": candle.high,
        "low": candle.low,
        "exitPrice": exit_price
    })
}

fn exit_level(
    trade: &VirtualTrade,
    event_price: Decimal,
) -> Option<(Decimal, VirtualTradeEventKind, &'static str)> {
    let target = active_tp2_edge(trade);
    match trade.direction {
        Direction::Buy => {
            if event_price <= trade.active_stop {
                Some((
                    trade.active_stop,
                    VirtualTradeEventKind::StopTouched,
                    if trade.active_stop == trade.initial_stop {
                        "initial stop touched"
                    } else {
                        "trailing stop touched"
                    },
                ))
            } else if event_price >= target {
                Some((
                    target,
                    VirtualTradeEventKind::TargetTouched,
                    "target touched",
                ))
            } else {
                None
            }
        }
        Direction::Sell => {
            if event_price >= trade.active_stop {
                Some((
                    trade.active_stop,
                    VirtualTradeEventKind::StopTouched,
                    if trade.active_stop == trade.initial_stop {
                        "initial stop touched"
                    } else {
                        "trailing stop touched"
                    },
                ))
            } else if event_price <= target {
                Some((
                    target,
                    VirtualTradeEventKind::TargetTouched,
                    "target touched",
                ))
            } else {
                None
            }
        }
        Direction::None => None,
    }
}

fn terminal_trade_payload(
    aggregate_trade_id: Option<i64>,
    event_price: Decimal,
    exit_price: Decimal,
) -> serde_json::Value {
    let mut payload = json!({
        "eventPrice": event_price,
        "exitPrice": exit_price
    });
    if let Some(aggregate_trade_id) = aggregate_trade_id {
        payload["aggregateTradeId"] = json!(aggregate_trade_id);
    } else {
        payload["source"] = json!("REFERENCE_PRICE");
    }
    payload
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecommendationExit {
    Expired,
    Invalidated(&'static str),
}

fn recommendation_exit(
    signal: &SignalDefinition,
    event_price: Decimal,
    event_time: i64,
) -> Option<RecommendationExit> {
    if signal
        .expires_at
        .is_some_and(|expires_at| event_time >= expires_at)
    {
        return Some(RecommendationExit::Expired);
    }
    match signal.direction {
        Direction::Buy => {
            if event_price >= signal.primary_target {
                Some(RecommendationExit::Invalidated(
                    "target touched before entry",
                ))
            } else if event_price <= signal.stop_loss {
                Some(RecommendationExit::Invalidated("stop touched before entry"))
            } else {
                None
            }
        }
        Direction::Sell => {
            if event_price <= signal.primary_target {
                Some(RecommendationExit::Invalidated(
                    "target touched before entry",
                ))
            } else if event_price >= signal.stop_loss {
                Some(RecommendationExit::Invalidated("stop touched before entry"))
            } else {
                None
            }
        }
        Direction::None => None,
    }
}

fn recommendation_ohlc_exit(
    signal: &SignalDefinition,
    candle: &Candle,
) -> Option<(RecommendationExit, Decimal)> {
    if signal
        .expires_at
        .is_some_and(|expires_at| candle.close_time >= expires_at)
    {
        return Some((RecommendationExit::Expired, candle.close));
    }
    let (stop_touched, target_touched) = match signal.direction {
        Direction::Buy => (
            candle.low <= signal.stop_loss,
            candle.high >= signal.primary_target,
        ),
        Direction::Sell => (
            candle.high >= signal.stop_loss,
            candle.low <= signal.primary_target,
        ),
        Direction::None => return None,
    };
    if stop_touched {
        return Some((
            RecommendationExit::Invalidated("stop touched before entry"),
            signal.stop_loss,
        ));
    }
    target_touched.then_some((
        RecommendationExit::Invalidated("target touched before entry"),
        signal.primary_target,
    ))
}

fn has_valid_signal_geometry(signal: &SignalDefinition) -> bool {
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

fn close_trade(mut trade: VirtualTrade, time: i64, price: Decimal, reason: &str) -> VirtualTrade {
    update_point_excursion(&mut trade, price);
    trade.trade_state = TradeState::Closed;
    trade.exit_time = Some(time);
    trade.exit_price = Some(price);
    trade.duration_ms = Some(time - trade.actual_entry_time);
    trade.exit_reason = Some(reason.to_owned());
    let risk = (trade.actual_entry_price - trade.initial_stop).abs();
    if risk > Decimal::ZERO {
        let exit_r = match trade.direction {
            Direction::Buy => (price - trade.actual_entry_price) / risk,
            Direction::Sell => (trade.actual_entry_price - price) / risk,
            Direction::None => Decimal::ZERO,
        };
        if trade.position_stage == PositionStage::Legacy {
            trade.gross_r = Some(exit_r.round_dp(6));
        } else {
            let weighted_r = (trade.realized_r + trade.remaining_fraction * exit_r).round_dp(6);
            trade.remaining_fraction = Decimal::ZERO;
            trade.realized_fraction = Decimal::ONE;
            trade.realized_r = weighted_r;
            trade.gross_r = Some(weighted_r);
        }
    }
    trade
}

fn partial_target_touched(trade: &VirtualTrade, price: Decimal) -> bool {
    if trade.position_stage != PositionStage::FullPosition {
        return false;
    }
    let Some(tp1_price) = trade.tp1_price else {
        return false;
    };
    match trade.direction {
        Direction::Buy => price >= tp1_price,
        Direction::Sell => price <= tp1_price,
        Direction::None => false,
    }
}

fn stop_touched(trade: &VirtualTrade, price: Decimal) -> bool {
    match trade.direction {
        Direction::Buy => price <= trade.active_stop,
        Direction::Sell => price >= trade.active_stop,
        Direction::None => false,
    }
}

fn ohlc_partial_target_touched(trade: &VirtualTrade, candle: &Candle) -> bool {
    if trade.position_stage != PositionStage::FullPosition {
        return false;
    }
    let Some(tp1_price) = trade.tp1_price else {
        return false;
    };
    match trade.direction {
        Direction::Buy => candle.high >= tp1_price,
        Direction::Sell => candle.low <= tp1_price,
        Direction::None => false,
    }
}

fn ohlc_stop_touched(trade: &VirtualTrade, candle: &Candle) -> bool {
    match trade.direction {
        Direction::Buy => candle.low <= trade.active_stop,
        Direction::Sell => candle.high >= trade.active_stop,
        Direction::None => false,
    }
}

fn activate_runner(
    mut trade: VirtualTrade,
    time: i64,
) -> Result<VirtualTrade, TrackerRuntimeError> {
    let (Some(tp1_r), Some(tp1_fraction), Some(_)) =
        (trade.tp1_r, trade.tp1_fraction, trade.tp1_price)
    else {
        return Err(TrackerRuntimeError::InvalidSignal(
            "FULL_POSITION trade requires TP1 geometry".to_owned(),
        ));
    };
    trade.position_stage = PositionStage::Runner;
    trade.tp1_filled_at = Some(time);
    trade.realized_fraction = tp1_fraction;
    trade.remaining_fraction = Decimal::ONE - tp1_fraction;
    trade.realized_r = (tp1_r * tp1_fraction).round_dp(6);
    trade.active_stop = match trade.direction {
        Direction::Buy => trade.active_stop.max(trade.actual_entry_price),
        Direction::Sell => trade.active_stop.min(trade.actual_entry_price),
        Direction::None => trade.active_stop,
    };
    Ok(trade)
}

fn update_point_excursion(trade: &mut VirtualTrade, price: Decimal) -> bool {
    update_excursion(trade, price, price)
}

fn update_ohlc_excursion(trade: &mut VirtualTrade, candle: &Candle) -> bool {
    match trade.direction {
        Direction::Buy => update_excursion(trade, candle.high, candle.low),
        Direction::Sell => update_excursion(trade, candle.low, candle.high),
        Direction::None => false,
    }
}

fn update_excursion(
    trade: &mut VirtualTrade,
    favorable_price: Decimal,
    adverse_price: Decimal,
) -> bool {
    let risk = (trade.actual_entry_price - trade.initial_stop).abs();
    if risk <= Decimal::ZERO {
        return false;
    }
    let (favorable_r, adverse_r) = match trade.direction {
        Direction::Buy => (
            (favorable_price - trade.actual_entry_price) / risk,
            (adverse_price - trade.actual_entry_price) / risk,
        ),
        Direction::Sell => (
            (trade.actual_entry_price - favorable_price) / risk,
            (trade.actual_entry_price - adverse_price) / risk,
        ),
        Direction::None => return false,
    };
    let next_mfe = trade.mfe_r.max(favorable_r).round_dp(6);
    let next_mae = trade.mae_r.min(adverse_r).round_dp(6);
    let changed = next_mfe != trade.mfe_r || next_mae != trade.mae_r;
    trade.mfe_r = next_mfe;
    trade.mae_r = next_mae;
    changed
}

fn append_virtual_trade_write(transition: &mut DurableTransition, trade: VirtualTrade) {
    let write = VirtualTradeWrite { trade };
    if transition.virtual_trade_write.is_none() {
        transition.virtual_trade_write = Some(write);
    } else {
        transition.virtual_trade_writes.push(write);
    }
}

fn valid_target_move(trade: &VirtualTrade, price: Decimal) -> bool {
    match trade.direction {
        Direction::Buy => price > trade.actual_entry_price,
        Direction::Sell => price < trade.actual_entry_price,
        Direction::None => false,
    }
}

fn valid_stop_move(trade: &VirtualTrade, price: Decimal) -> bool {
    match trade.direction {
        Direction::Buy => {
            price > trade.active_stop
                && (trade.position_stage != PositionStage::Runner
                    || price >= trade.actual_entry_price)
        }
        Direction::Sell => {
            price < trade.active_stop
                && (trade.position_stage != PositionStage::Runner
                    || price <= trade.actual_entry_price)
        }
        Direction::None => false,
    }
}

fn directive_matches_trade_version_and_order(
    trade: &VirtualTrade,
    directive: &ManagementDirective,
) -> bool {
    directive.algorithm_version == trade.versions.algorithm_version
        && directive.parameter_version == trade.versions.parameter_version
        && trade
            .last_management_effective_at
            .is_none_or(|last| directive.effective_at >= last)
        && trade
            .last_management_candle_time
            .is_none_or(|last| directive.source_closed_candle_time >= last)
}

fn ignored_directive_transition(
    trade: &VirtualTrade,
    directive: &ManagementDirective,
    reason: &str,
) -> DurableTransition {
    DurableTransition {
        consumed_directive_ids: vec![directive.directive_id],
        monitoring_events: vec![monitoring_event(
            "TRADE_DIRECTIVE_IGNORED",
            "virtual_trade",
            Some(trade.virtual_trade_id),
            extend_virtual_trade_payload(
                trade,
                json!({
                    "directiveId": directive.directive_id,
                    "directiveKind": directive.kind.as_str(),
                    "reason": reason,
                    "requestedPrice": directive.price,
                    "activeStop": trade.active_stop,
                    "sourceClosedCandleTime": directive.source_closed_candle_time,
                    "lastManagementCandleTime": trade.last_management_candle_time,
                }),
            ),
        )],
        ..DurableTransition::default()
    }
}

fn extend_virtual_trade_payload(
    trade: &VirtualTrade,
    event_fields: serde_json::Value,
) -> serde_json::Value {
    let mut payload = virtual_trade_payload(trade);
    if let (Some(payload), Some(event_fields)) = (payload.as_object_mut(), event_fields.as_object())
    {
        payload.extend(event_fields.clone());
    }
    payload
}

fn close_directive_payload(trade: &VirtualTrade, kind: DirectiveKind) -> serde_json::Value {
    let event_fields = if matches!(
        kind,
        DirectiveKind::SuggestClose | DirectiveKind::EmergencyExit
    ) {
        json!({ "managementAction": kind.as_str() })
    } else {
        json!({})
    };
    extend_virtual_trade_payload(trade, event_fields)
}

fn monitoring_event(
    event_type: &str,
    entity_type: &str,
    entity_id: Option<Uuid>,
    payload: serde_json::Value,
) -> NewMonitoringEvent {
    NewMonitoringEvent {
        event_id: Uuid::now_v7(),
        event_type: event_type.to_owned(),
        entity_type: entity_type.to_owned(),
        entity_id,
        payload,
    }
}

#[cfg(test)]
mod bookkeeping_tests {
    use super::{TrackerInput, TrackerRuntime};
    use analysis_domain::{Direction, ExecutionInstruction, PlanKind, PriceZone, SignalDefinition};
    use market_domain::{AggregateTrade, Symbol, Timeframe};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn signal(effective_at: i64) -> SignalDefinition {
        SignalDefinition {
            signal_id: Uuid::now_v7(),
            symbol: Symbol::new("XAUUSDT").unwrap(),
            plan: PlanKind::Ict,
            target_timeframe: Timeframe::M1,
            evaluated_at: effective_at,
            direction: Direction::Buy,
            execution_instruction: ExecutionInstruction::LimitZone,
            entry_zone: Some(PriceZone::new(Decimal::new(4100, 0), Decimal::new(4110, 0)).unwrap()),
            projected_entry: Some(Decimal::new(4105, 0)),
            stop_loss: Decimal::new(4090, 0),
            primary_target: Decimal::new(4130, 0),
            execution_plan: None,
            reference_levels: Vec::new(),
            risk_reward: Decimal::TWO,
            pre_entry_validation: None,
            confidence: Decimal::ONE,
            evidence: Vec::new(),
            htf_context: None,
            effective_at,
            detected_at: effective_at,
            published_at: effective_at,
            release_id: Uuid::nil(),
            algorithm_version: "algo-v1".to_owned(),
            parameter_version: "param-v1".to_owned(),
            created_at: effective_at,
            expires_at: None,
        }
    }

    fn trade(id: i64, price: i64) -> AggregateTrade {
        AggregateTrade::new(
            Symbol::new("XAUUSDT").unwrap(),
            id,
            id,
            id,
            id * 100,
            id * 100,
            Decimal::new(price, 0),
        )
        .unwrap()
    }

    #[test]
    fn replaced_trades_do_not_accumulate_bookkeeping() {
        let mut runtime = TrackerRuntime::default();
        for round in 0..20_i64 {
            let base = 10 + round * 10;
            runtime.apply(TrackerInput::RecommendationOpened(signal(base * 100)));
            runtime.apply(TrackerInput::AggregateTrade(trade(base, 4105)));
            runtime.apply(TrackerInput::AggregateTrade(trade(base + 1, 4089)));
        }

        assert!(runtime.entry_feedback_sent.len() <= 1);
        assert!(runtime.close_feedback_sent.len() <= 1);
        assert!(runtime.next_trade_sequence.len() <= 1);
    }
}
