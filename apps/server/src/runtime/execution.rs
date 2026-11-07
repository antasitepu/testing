use super::{
    current_unix_millis,
    health::RuntimeHealth,
    recovery::{latest_m1_close_time, recover_execution_tracking, replay_open_trades_through},
};
use crate::execution_wake::{ExecutionBarrierRequest, ExecutionWakeSource};
use analysis_domain::{
    ExecutionInstruction, ManagementDirective, SignalDefinition, SignalEvent, SignalEventKind,
};
use execution_service::{
    ExecutionError, ExecutionSupervisor, MarketCursor, SupervisorEvent, SupervisorStep,
    TrackerStore, signal_recommendation_payload,
};
use market_domain::{MarketEvent, SessionStatus, Symbol, Timeframe};
use market_service::{MarketSource, TradableCandlePolicy};
use market_storage::{CandleRepository, MarketRepository, SessionRepository};
use runtime_observability::{ActivityKind, record_activity, record_subscriber_lag};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, watch};

const EXECUTION_INPUT_FALLBACK_POLL: Duration = Duration::from_secs(10);
/// Longer than any closure in the session history, which peaks at about 50 hours.
const NEXT_OPEN_SESSION_LOOKAHEAD_MS: i64 = 14 * 86_400_000;

pub struct ExecutionRuntime {
    pub symbol: Symbol,
    pub pool: PgPool,
    pub rest: Arc<dyn MarketSource>,
    pub candles: Arc<CandleRepository>,
    pub tracker: Arc<dyn TrackerStore>,
    pub market_events: broadcast::Receiver<MarketEvent>,
    pub input_wake: watch::Receiver<u64>,
    pub barrier_requests: mpsc::Receiver<ExecutionBarrierRequest>,
    pub monitoring_wake: ExecutionWakeSource,
    pub shutdown: watch::Receiver<bool>,
    pub health: RuntimeHealth,
}

pub async fn run_execution_loop(runtime: ExecutionRuntime) -> Result<(), ExecutionError> {
    let ExecutionRuntime {
        symbol: execution_symbol,
        pool: execution_pool,
        rest: execution_rest,
        candles: execution_candles,
        tracker: execution_tracker,
        market_events: mut execution_events,
        input_wake: mut execution_input_wake,
        barrier_requests: mut execution_barrier_requests,
        monitoring_wake: execution_monitoring_wake,
        shutdown: mut execution_shutdown,
        health: execution_health,
    } = runtime;
    let mut supervisor = ExecutionSupervisor::new(execution_tracker);
    supervisor.restore_active(&execution_symbol).await?;
    refresh_market_closures(&mut supervisor, &execution_pool, &execution_symbol).await?;
    process_active_next_open_canonical_opens(&mut supervisor, &execution_pool).await?;
    execution_health.increment_recovery_count(current_unix_millis());
    recover_execution_tracking(
        &mut supervisor,
        execution_rest.clone(),
        &execution_pool,
        &execution_symbol,
    )
    .await?;
    repair_terminal_recommendation_states_at_latest_close(
        &execution_pool,
        execution_candles.clone(),
        &execution_symbol,
        Some(&execution_monitoring_wake),
    )
    .await?;
    let mut delivered_signals = HashSet::new();
    for signal in active_execution_signal_ids(&execution_pool).await? {
        delivered_signals.insert(signal);
    }
    let mut delivered_directives = HashSet::new();
    process_pending_execution_inputs(
        &mut supervisor,
        &execution_pool,
        execution_candles.clone(),
        &execution_symbol,
        &mut delivered_signals,
        &mut delivered_directives,
        Some(&execution_monitoring_wake),
    )
    .await?;
    let mut input_fallback_poll = tokio::time::interval(EXECUTION_INPUT_FALLBACK_POLL);
    input_fallback_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_fallback_poll.tick().await;
    loop {
        if *execution_shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            changed = execution_shutdown.changed() => {
                if changed.is_ok() && *execution_shutdown.borrow() {
                    return Ok(());
                }
            }
            event = execution_events.recv() => {
                match event {
                    Ok(MarketEvent::ResyncRequired { reason, .. }) => {
                        tracing::warn!(%reason, "execution market resync requested");
                        execution_events = execution_events.resubscribe();
                        execution_health.increment_recovery_count(current_unix_millis());
                        recover_execution_tracking(
                            &mut supervisor,
                            execution_rest.clone(),
                            &execution_pool,
                            &execution_symbol,
                        )
                        .await?;
                        record_activity(ActivityKind::Execution, current_unix_millis());
                    }
                    // The market supervisor stores a session before it publishes this event.
                    Ok(MarketEvent::SessionChanged(_)) => {
                        refresh_market_closures(
                            &mut supervisor,
                            &execution_pool,
                            &execution_symbol,
                        )
                        .await?;
                        record_activity(ActivityKind::Execution, current_unix_millis());
                    }
                    Ok(MarketEvent::AggregateTrade(trade)) => {
                        let step = supervisor
                            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                                trade.clone(),
                            )))
                            .await?;
                        if matches!(step, SupervisorStep::GapDetected | SupervisorStep::Frozen) {
                            tracing::warn!(?step, "execution aggregate trade requires recovery");
                            execution_events = execution_events.resubscribe();
                            execution_health.increment_recovery_count(current_unix_millis());
                            recover_execution_tracking(
                                &mut supervisor,
                                execution_rest.clone(),
                                &execution_pool,
                                &execution_symbol,
                            )
                            .await?;
                            let mut retry = supervisor
                                .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                                    trade.clone(),
                                )))
                                .await?;
                            if retry == SupervisorStep::GapDetected {
                                // Exact REST history is often truncated past Binance
                                // retention. Rebase only after recovery already ran,
                                // and log the skipped range for auditability.
                                let previous_id = trade.aggregate_trade_id.checked_sub(1).ok_or_else(|| {
                                    ExecutionError::InvalidData(
                                        "cannot rebase before aggregate trade zero".to_owned(),
                                    )
                                })?;
                                if let Some(seen) = supervisor.last_seen_cursor() {
                                    if seen.symbol == trade.symbol
                                        && previous_id > seen.last_processed_agg_trade_id
                                    {
                                        tracing::warn!(
                                            symbol = %trade.symbol,
                                            from = seen.last_processed_agg_trade_id + 1,
                                            to = previous_id,
                                            "rebasing execution cursor past unrecovered aggregate trades after OHLC recovery"
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        symbol = %trade.symbol,
                                        to = previous_id,
                                        "rebasing execution cursor after OHLC recovery without prior checkpoint"
                                    );
                                }
                                let rebase = supervisor
                                    .rebase_market_cursor(MarketCursor {
                                        symbol: trade.symbol.clone(),
                                        last_processed_agg_trade_id: previous_id,
                                        last_trade_time: trade.trade_time.saturating_sub(1),
                                    })
                                    .await?;
                                if rebase == SupervisorStep::Frozen {
                                    return Err(ExecutionError::Storage {
                                        operation: "rebase execution market cursor",
                                        message: "cursor rebase failed after OHLC recovery".to_owned(),
                                    });
                                }
                                retry = supervisor
                                    .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                                        trade,
                                    )))
                                    .await?;
                            }
                            if matches!(retry, SupervisorStep::GapDetected | SupervisorStep::Frozen) {
                                return Err(ExecutionError::InvalidData(
                                    "execution aggregate trade remained unrecoverable".to_owned(),
                                ));
                            }
                            let _ = supervisor.checkpoint_market_cursor().await?;
                        }
                        record_activity(ActivityKind::Execution, current_unix_millis());
                    }
                    Ok(event) => {
                        let step = supervisor.process(SupervisorEvent::Market(event)).await?;
                        if matches!(step, SupervisorStep::GapDetected | SupervisorStep::Frozen) {
                            tracing::warn!(?step, "execution market processing requires recovery");
                            execution_events = execution_events.resubscribe();
                            execution_health.increment_recovery_count(current_unix_millis());
                            recover_execution_tracking(
                                &mut supervisor,
                                execution_rest.clone(),
                                &execution_pool,
                                &execution_symbol,
                            )
                            .await?;
                        }
                        record_activity(ActivityKind::Execution, current_unix_millis());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        record_subscriber_lag();
                        tracing::warn!(skipped, "execution supervisor lagged market events");
                        execution_events = execution_events.resubscribe();
                        execution_health.increment_recovery_count(current_unix_millis());
                        recover_execution_tracking(
                            &mut supervisor,
                            execution_rest.clone(),
                            &execution_pool,
                            &execution_symbol,
                        )
                        .await?;
                        record_activity(ActivityKind::Execution, current_unix_millis());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            changed = execution_input_wake.changed() => {
                if changed.is_ok() {
                    process_pending_execution_inputs(
                        &mut supervisor,
                        &execution_pool,
                        execution_candles.clone(),
                        &execution_symbol,
                        &mut delivered_signals,
                        &mut delivered_directives,
                        Some(&execution_monitoring_wake),
                    )
                    .await?;
                }
            }
            Some(request) = execution_barrier_requests.recv() => {
                // The order of a live session: decisions from earlier bars take effect, then
                // the candles up to the bar analysis is about to evaluate arrive.
                process_pending_execution_inputs(
                    &mut supervisor,
                    &execution_pool,
                    execution_candles.clone(),
                    &execution_symbol,
                    &mut delivered_signals,
                    &mut delivered_directives,
                    Some(&execution_monitoring_wake),
                )
                .await?;
                let reached = replay_open_trades_through(
                    &mut supervisor,
                    &execution_pool,
                    &execution_symbol,
                    request.close_time,
                )
                .await?;
                let _ = request.done.send(reached);
                record_activity(ActivityKind::Execution, current_unix_millis());
            }
            _ = input_fallback_poll.tick() => {
                process_pending_execution_inputs(
                    &mut supervisor,
                    &execution_pool,
                    execution_candles.clone(),
                    &execution_symbol,
                    &mut delivered_signals,
                    &mut delivered_directives,
                    Some(&execution_monitoring_wake),
                )
                .await?;
            }
        }
    }
}

pub async fn active_execution_signal_ids(pool: &PgPool) -> Result<Vec<uuid::Uuid>, ExecutionError> {
    sqlx::query_scalar(
        "SELECT signal_id FROM tracker_states \
         WHERE state->>'setupState' = 'ENTRY_AVAILABLE' \
            OR state->>'tradeState' = 'OPEN' \
         ORDER BY signal_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load active execution signal ids",
        message: error.to_string(),
    })
}

async fn process_latest_reference_price(
    supervisor: &mut ExecutionSupervisor,
    candles: Arc<CandleRepository>,
    symbol: &Symbol,
) -> Result<(), ExecutionError> {
    let latest = candles
        .latest(symbol, Timeframe::M1, None, 1)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "load latest candle for execution reference",
            message: error.to_string(),
        })?
        .into_iter()
        .next();
    let Some(candle) = latest else {
        return Ok(());
    };
    process_reference_price(supervisor, symbol, candle.close_time, candle.close).await
}

pub(super) async fn process_pending_execution_inputs(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
    candles: Arc<CandleRepository>,
    symbol: &Symbol,
    delivered_signals: &mut HashSet<uuid::Uuid>,
    delivered_directives: &mut HashSet<uuid::Uuid>,
    monitoring_wake: Option<&ExecutionWakeSource>,
) -> Result<(), ExecutionError> {
    let mut accepted_signals = Vec::new();
    for signal in pending_signals(pool, 100).await? {
        if delivered_signals.contains(&signal.signal_id) {
            continue;
        }
        let committed_signal = signal.clone();
        let step = supervisor
            .process(SupervisorEvent::SignalOpened(Box::new(signal)))
            .await?;
        match step {
            SupervisorStep::Committed => {
                delivered_signals.insert(committed_signal.signal_id);
                accepted_signals.push(committed_signal);
                record_activity(ActivityKind::Execution, current_unix_millis());
            }
            SupervisorStep::Frozen => supervisor.restore_active(symbol).await?,
            SupervisorStep::GapDetected | SupervisorStep::Ignored | SupervisorStep::Cancelled => {}
        }
    }
    if !accepted_signals.is_empty() {
        for signal in &accepted_signals {
            process_stored_next_open_canonical_open(supervisor, pool, signal).await?;
        }
        process_latest_reference_price(supervisor, candles.clone(), symbol).await?;
        repair_terminal_recommendation_states_at_latest_close(
            pool,
            candles.clone(),
            symbol,
            monitoring_wake,
        )
        .await?;
    }

    let now_ms = current_unix_millis();
    for directive in pending_directives(pool, now_ms, 100).await? {
        if delivered_directives.contains(&directive.directive_id) {
            continue;
        }
        let directive_id = directive.directive_id;
        match supervisor
            .process(SupervisorEvent::Directive(directive))
            .await?
        {
            SupervisorStep::Committed => {
                delivered_directives.insert(directive_id);
                record_activity(ActivityKind::Execution, current_unix_millis());
            }
            SupervisorStep::Frozen => supervisor.restore_active(symbol).await?,
            SupervisorStep::GapDetected | SupervisorStep::Ignored | SupervisorStep::Cancelled => {}
        }
    }
    Ok(())
}

pub(super) async fn process_active_next_open_canonical_opens(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
) -> Result<(), ExecutionError> {
    for signal in active_next_open_recommendations(pool).await? {
        process_stored_next_open_canonical_open(supervisor, pool, &signal).await?;
    }
    Ok(())
}

async fn process_stored_next_open_canonical_open(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
    signal: &SignalDefinition,
) -> Result<(), ExecutionError> {
    if signal.execution_instruction != ExecutionInstruction::NextOpen {
        return Ok(());
    }
    let entry_open = next_open_entry_time(pool, signal).await?;
    if entry_open > signal.effective_at {
        supervisor.delay_next_open(signal.signal_id, entry_open);
    }
    let Some(candle) =
        load_first_candle_at_or_after(pool, &signal.symbol, signal.target_timeframe, entry_open)
            .await?
    else {
        return Ok(());
    };
    let entry_candle_close = candle.close_time;
    let _ = supervisor
        .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(candle)))
        .await?;
    if latest_m1_close_time(pool, &signal.symbol)
        .await?
        .is_some_and(|latest| entry_candle_close < latest)
    {
        supervisor.begin_replay(signal.signal_id, entry_candle_close);
    }
    Ok(())
}

/// Gives the tracker the stored closures. A market with no stored sessions counts as open.
pub(super) async fn refresh_market_closures(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<(), ExecutionError> {
    let storage_error = |error: market_storage::StorageError| ExecutionError::Storage {
        operation: "load market closures",
        message: error.to_string(),
    };
    let Some(market) = MarketRepository::new(pool.clone())
        .load(symbol)
        .await
        .map_err(storage_error)?
    else {
        return Ok(());
    };
    let closures = SessionRepository::new(pool.clone())
        .between(&market.session_market, 0, i64::MAX)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|session| session.status == SessionStatus::NoTrading)
        .map(|session| (session.start_time, session.end_time))
        .collect();
    supervisor.set_market_closures(closures);
    Ok(())
}

async fn market_closed_at(
    pool: &PgPool,
    symbol: &Symbol,
    time: i64,
) -> Result<bool, ExecutionError> {
    let storage_error = |error: market_storage::StorageError| ExecutionError::Storage {
        operation: "load session at latest close",
        message: error.to_string(),
    };
    let Some(market) = MarketRepository::new(pool.clone())
        .load(symbol)
        .await
        .map_err(storage_error)?
    else {
        return Ok(false);
    };
    let session = SessionRepository::new(pool.clone())
        .covering(&market.session_market, time)
        .await
        .map_err(storage_error)?;
    Ok(session.is_some_and(|session| session.status == SessionStatus::NoTrading))
}

/// `NO_TRADING` delays a next open to the first tradable bar, which is also where the backtest
/// fills. Without session coverage the signal keeps its own boundary.
async fn next_open_entry_time(
    pool: &PgPool,
    signal: &SignalDefinition,
) -> Result<i64, ExecutionError> {
    let storage_error = |error: market_storage::StorageError| ExecutionError::Storage {
        operation: "load sessions for next open",
        message: error.to_string(),
    };
    let Some(market) = MarketRepository::new(pool.clone())
        .load(&signal.symbol)
        .await
        .map_err(storage_error)?
    else {
        return Ok(signal.effective_at);
    };
    let sessions = SessionRepository::new(pool.clone())
        .between(
            &market.session_market,
            signal.effective_at,
            signal
                .effective_at
                .saturating_add(NEXT_OPEN_SESSION_LOOKAHEAD_MS),
        )
        .await
        .map_err(storage_error)?;
    let entry_open = TradableCandlePolicy.first_tradable_open(
        signal.target_timeframe,
        signal.effective_at,
        &sessions,
    );
    if entry_open.is_none() {
        tracing::warn!(signal_id = %signal.signal_id, "next-open entry has no session coverage");
    }
    Ok(entry_open.unwrap_or(signal.effective_at))
}

async fn load_first_candle_at_or_after(
    pool: &PgPool,
    symbol: &Symbol,
    timeframe: Timeframe,
    open_time: i64,
) -> Result<Option<market_domain::Candle>, ExecutionError> {
    let row = sqlx::query(
        "SELECT symbol, interval, open_time, close_time, open, high, low, close \
         FROM candles \
         WHERE symbol = $1 AND interval = $2 AND open_time >= $3 \
         ORDER BY open_time ASC \
         LIMIT 1",
    )
    .bind(symbol.as_str())
    .bind(timeframe.as_str())
    .bind(open_time)
    .fetch_optional(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load stored next-open candle",
        message: error.to_string(),
    })?;
    row.map(|row| {
        market_domain::Candle::new(
            Symbol::new(row.get::<String, _>("symbol"))
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
            timeframe,
            row.get("open_time"),
            row.get("close_time"),
            row.get("open"),
            row.get("high"),
            row.get("low"),
            row.get("close"),
        )
        .map_err(|error| ExecutionError::InvalidData(error.to_string()))
    })
    .transpose()
}

async fn active_next_open_recommendations(
    pool: &PgPool,
) -> Result<Vec<SignalDefinition>, ExecutionError> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (s.symbol, s.plan, s.interval) s.definition \
         FROM signals s \
         JOIN tracker_states ts ON ts.signal_id = s.signal_id \
         LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
         WHERE ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
           AND ts.state->>'tradeState' = 'NOT_ENTERED' \
           AND vt.signal_id IS NULL \
           AND s.definition->>'executionInstruction' = 'NEXT_OPEN' \
         ORDER BY s.symbol, s.plan, s.interval, s.created_at DESC, s.signal_id DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load active next-open recommendations",
        message: error.to_string(),
    })?;
    rows.into_iter()
        .map(|row| {
            serde_json::from_value(row.get("definition"))
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))
        })
        .collect()
}

pub(super) async fn repair_terminal_recommendation_states_at_latest_close(
    pool: &PgPool,
    candles: Arc<CandleRepository>,
    symbol: &Symbol,
    monitoring_wake: Option<&ExecutionWakeSource>,
) -> Result<(), ExecutionError> {
    let latest = candles
        .latest(symbol, Timeframe::M1, None, 1)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "load latest candle for execution state repair",
            message: error.to_string(),
        })?
        .into_iter()
        .next();
    let Some(candle) = latest else {
        return Ok(());
    };
    // A setup's time still runs out during a closure, but prices there do not judge it.
    let market_closed = market_closed_at(pool, symbol, candle.close_time).await?;
    let rows = sqlx::query(
        "WITH terminal AS ( \
             SELECT ts.signal_id, s.definition, \
                    CASE \
                        WHEN (s.definition->>'expiresAt') IS NOT NULL \
                         AND (s.definition->>'expiresAt')::BIGINT <= $2 THEN 'EXPIRED' \
                        ELSE 'INVALIDATED' \
                    END AS setup_state, \
                    CASE \
                        WHEN (s.definition->>'expiresAt') IS NOT NULL \
                         AND (s.definition->>'expiresAt')::BIGINT <= $2 THEN 'entry expired' \
                        WHEN s.definition->>'direction' = 'BUY' \
                         AND $3::NUMERIC >= (s.definition->>'primaryTarget')::NUMERIC THEN 'target touched before entry' \
                        WHEN s.definition->>'direction' = 'BUY' \
                         AND $3::NUMERIC <= (s.definition->>'stopLoss')::NUMERIC THEN 'stop touched before entry' \
                        WHEN s.definition->>'direction' = 'SELL' \
                         AND $3::NUMERIC <= (s.definition->>'primaryTarget')::NUMERIC THEN 'target touched before entry' \
                        ELSE 'stop touched before entry' \
                    END AS reason \
             FROM tracker_states ts \
             JOIN signals s ON s.signal_id = ts.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE s.symbol = $1 \
               AND ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
               AND (s.definition->>'effectiveAt')::BIGINT <= $2 \
               AND ( \
                    ((s.definition->>'expiresAt') IS NOT NULL \
                     AND (s.definition->>'expiresAt')::BIGINT <= $2) \
                    OR (NOT $4::BOOLEAN AND s.definition->>'direction' = 'BUY' \
                        AND ($3::NUMERIC >= (s.definition->>'primaryTarget')::NUMERIC \
                             OR $3::NUMERIC <= (s.definition->>'stopLoss')::NUMERIC)) \
                    OR (NOT $4::BOOLEAN AND s.definition->>'direction' = 'SELL' \
                        AND ($3::NUMERIC <= (s.definition->>'primaryTarget')::NUMERIC \
                             OR $3::NUMERIC >= (s.definition->>'stopLoss')::NUMERIC)) \
               ) \
         ) SELECT signal_id, definition, setup_state, reason FROM terminal",
    )
    .bind(symbol.as_str())
    .bind(candle.close_time)
    .bind(candle.close)
    .bind(market_closed)
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load terminal recommendation repairs",
        message: error.to_string(),
    })?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "begin terminal recommendation repair",
            message: error.to_string(),
        })?;
    let mut repaired = 0_u64;
    for row in rows {
        let signal_id: uuid::Uuid = row.get("signal_id");
        let definition: Value = row.get("definition");
        let signal: SignalDefinition = serde_json::from_value(definition)
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
        let setup_state: String = row.get("setup_state");
        let reason: String = row.get("reason");
        let updated = sqlx::query(
            "UPDATE tracker_states \
             SET state = jsonb_build_object( \
                     'signalId', signal_id, \
                     'setupState', $2::TEXT, \
                     'tradeState', 'NOT_ENTERED', \
                     'reason', $3::TEXT, \
                     'price', $4::NUMERIC \
                 ), \
                 tracking_health = 'LIVE', \
                 updated_at = NOW() \
             WHERE signal_id = $1 \
               AND state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND state->>'tradeState' = 'NOT_ENTERED'",
        )
        .bind(signal_id)
        .bind(&setup_state)
        .bind(&reason)
        .bind(candle.close)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "repair terminal recommendation state",
            message: error.to_string(),
        })?;
        if updated.rows_affected() == 0 {
            continue;
        }
        repaired += updated.rows_affected();
        let event_kind = if setup_state == "EXPIRED" {
            SignalEventKind::Expired
        } else {
            SignalEventKind::Invalidated
        };
        let signal_event = SignalEvent {
            event_id: uuid::Uuid::now_v7(),
            signal_id,
            sequence: 1,
            kind: event_kind,
            effective_at: candle.close_time,
            recorded_at: candle.close_time,
            virtual_trade_id: None,
            price: Some(candle.close),
            reason: Some(reason.clone()),
        };
        sqlx::query(
            "INSERT INTO signal_events \
             (event_id, signal_id, sequence, event_type, payload, effective_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (signal_id, sequence) DO NOTHING",
        )
        .bind(signal_event.event_id)
        .bind(signal_event.signal_id)
        .bind(signal_event.sequence)
        .bind(signal_event.kind.as_str())
        .bind(
            serde_json::to_value(&signal_event)
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
        )
        .bind(signal_event.effective_at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "insert terminal recommendation signal event",
            message: error.to_string(),
        })?;
        let monitoring_type = if event_kind == SignalEventKind::Expired {
            "RECOMMENDATION_EXPIRED"
        } else {
            "RECOMMENDATION_INVALIDATED"
        };
        let mut monitoring_payload = signal_recommendation_payload(&signal);
        monitoring_payload["condition"] = json!(setup_state);
        monitoring_payload["setupState"] = json!(setup_state);
        monitoring_payload["tradeState"] = json!("NOT_ENTERED");
        monitoring_payload["reason"] = json!(reason);
        monitoring_payload["price"] = json!(candle.close);
        sqlx::query(
            "INSERT INTO monitoring_events \
             (event_id, event_type, entity_type, entity_id, payload) \
             VALUES ($1, $2, 'signal', $3, $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(monitoring_type)
        .bind(signal_id)
        .bind(monitoring_payload)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "insert terminal recommendation monitoring event",
            message: error.to_string(),
        })?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "commit terminal recommendation repair",
            message: error.to_string(),
        })?;
    if repaired > 0 {
        if let Some(wake) = monitoring_wake {
            wake.wake();
        }
        tracing::debug!(
            symbol = %symbol,
            time = candle.close_time,
            price = %candle.close,
            repaired,
            "repaired terminal recommendation tracker states"
        );
    }
    Ok(())
}

async fn process_reference_price(
    supervisor: &mut ExecutionSupervisor,
    symbol: &Symbol,
    time: i64,
    price: Decimal,
) -> Result<(), ExecutionError> {
    let step = supervisor
        .process(SupervisorEvent::ReferencePrice {
            symbol: symbol.clone(),
            time,
            price,
        })
        .await?;
    tracing::debug!(%symbol, time, %price, ?step, "processed execution reference price");
    Ok(())
}

pub async fn pending_signals(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SignalDefinition>, ExecutionError> {
    if !(1..=1_000).contains(&limit) {
        return Err(ExecutionError::InvalidData(
            "pending execution signal limit must be 1..=1000".to_owned(),
        ));
    }
    let rows = sqlx::query(
        "SELECT s.definition \
         FROM signals s \
         LEFT JOIN tracker_states ts ON ts.signal_id = s.signal_id \
         LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
         WHERE vt.signal_id IS NULL \
           AND ts.signal_id IS NULL \
         ORDER BY s.created_at, s.signal_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load pending execution signals",
        message: error.to_string(),
    })?;
    rows.into_iter()
        .map(|row| {
            serde_json::from_value(row.get("definition"))
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))
        })
        .collect()
}

pub async fn pending_directives(
    pool: &PgPool,
    now_ms: i64,
    limit: i64,
) -> Result<Vec<ManagementDirective>, ExecutionError> {
    if !(1..=1_000).contains(&limit) || now_ms < 0 {
        return Err(ExecutionError::InvalidData(
            "pending execution directive limit/time is invalid".to_owned(),
        ));
    }
    let rows = sqlx::query(PENDING_EXECUTION_DIRECTIVES_SQL)
        .bind(now_ms)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|error| ExecutionError::Storage {
            operation: "load pending execution directives",
            message: error.to_string(),
        })?;
    rows.into_iter()
        .map(|row| {
            let mut directive: ManagementDirective = serde_json::from_value(row.get("payload"))
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))?;
            if directive.price.is_none() {
                directive.price = row.get("canonical_open");
            }
            Ok(directive)
        })
        .collect()
}

const PENDING_EXECUTION_DIRECTIVES_SQL: &str = "WITH active_signal_ids AS ( \
         SELECT signal_id FROM tracker_states \
         WHERE state->>'setupState' = 'ENTRY_AVAILABLE' \
           AND state->>'tradeState' = 'NOT_ENTERED' \
         UNION \
         SELECT signal_id FROM virtual_trades WHERE trade_state = 'OPEN' \
     ) \
     SELECT d.payload, c.open AS canonical_open \
     FROM active_signal_ids active \
     JOIN LATERAL ( \
         SELECT * FROM trade_management_directives d \
         WHERE d.signal_id = active.signal_id \
           AND d.consumed_at IS NULL \
           AND d.effective_at <= $1 \
         ORDER BY d.effective_at, d.directive_id \
         LIMIT $2 \
     ) d ON TRUE \
     JOIN signals s ON s.signal_id = d.signal_id \
     LEFT JOIN candles c \
       ON c.symbol = s.symbol \
      AND c.interval = s.interval \
      AND c.open_time = d.effective_at \
     WHERE (d.kind NOT IN ('EXIT_AT_NEXT_OPEN', 'REVERSE_AT_NEXT_OPEN', 'SUGGEST_CLOSE', 'EMERGENCY_EXIT') \
            OR (d.payload->>'price') IS NOT NULL \
            OR c.open IS NOT NULL) \
     ORDER BY d.effective_at, d.directive_id LIMIT $2";

const PENDING_EXECUTION_DIRECTIVES_EXPLAIN_SQL: &str = concat!(
    "EXPLAIN (ANALYZE, FORMAT JSON) ",
    "WITH active_signal_ids AS ( \
         SELECT signal_id FROM tracker_states \
         WHERE state->>'setupState' = 'ENTRY_AVAILABLE' \
           AND state->>'tradeState' = 'NOT_ENTERED' \
         UNION \
         SELECT signal_id FROM virtual_trades WHERE trade_state = 'OPEN' \
     ) \
     SELECT d.payload, c.open AS canonical_open \
     FROM active_signal_ids active \
     JOIN LATERAL ( \
         SELECT * FROM trade_management_directives d \
         WHERE d.signal_id = active.signal_id \
           AND d.consumed_at IS NULL \
           AND d.effective_at <= $1 \
         ORDER BY d.effective_at, d.directive_id \
         LIMIT $2 \
     ) d ON TRUE \
     JOIN signals s ON s.signal_id = d.signal_id \
     LEFT JOIN candles c \
       ON c.symbol = s.symbol \
      AND c.interval = s.interval \
      AND c.open_time = d.effective_at \
     WHERE (d.kind NOT IN ('EXIT_AT_NEXT_OPEN', 'REVERSE_AT_NEXT_OPEN', 'SUGGEST_CLOSE', 'EMERGENCY_EXIT') \
            OR (d.payload->>'price') IS NOT NULL \
            OR c.open IS NOT NULL) \
     ORDER BY d.effective_at, d.directive_id LIMIT $2"
);

pub fn pending_execution_directives_explain_sql() -> &'static str {
    PENDING_EXECUTION_DIRECTIVES_EXPLAIN_SQL
}
