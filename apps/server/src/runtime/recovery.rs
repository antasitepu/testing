use execution_service::{ExecutionError, ExecutionSupervisor, SupervisorEvent, SupervisorStep};
use market_domain::{MarketEvent, Symbol, Timeframe};
use market_service::MarketSource;
use runtime_observability::{RecoverySource, record_execution_recovery};
use sqlx::{PgPool, Row};
use std::sync::Arc;

pub(crate) async fn replay_unprocessed_aggregate_trades(
    supervisor: &mut ExecutionSupervisor,
    rest: Arc<dyn MarketSource>,
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<(), ExecutionError> {
    let Some(mut from_id) = active_market_cursor(pool, symbol)
        .await?
        .map(|cursor| cursor + 1)
    else {
        return Ok(());
    };
    loop {
        let trades = rest
            .aggregate_trades_from(symbol, from_id, 1_000)
            .await
            .map_err(|error| ExecutionError::Storage {
                operation: "replay aggregate trades",
                message: error.to_string(),
            })?;
        if trades.is_empty() {
            let _ = supervisor.checkpoint_market_cursor().await?;
            return Ok(());
        }
        for trade in &trades {
            let step = supervisor
                .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                    trade.clone(),
                )))
                .await?;
            if step == SupervisorStep::GapDetected {
                return Err(ExecutionError::InvalidData(format!(
                    "aggregate trade replay gap before id {}",
                    trade.aggregate_trade_id
                )));
            }
            if step == SupervisorStep::Frozen {
                return Err(ExecutionError::Storage {
                    operation: "replay aggregate trades",
                    message: "execution supervisor froze during replay".to_owned(),
                });
            }
        }
        let Some(last) = trades.last() else {
            return Ok(());
        };
        from_id = last.aggregate_trade_id + 1;
        if trades.len() < 1_000 {
            let _ = supervisor.checkpoint_market_cursor().await?;
            return Ok(());
        }
    }
}

pub(crate) async fn recover_execution_tracking(
    supervisor: &mut ExecutionSupervisor,
    rest: Arc<dyn MarketSource>,
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<(), ExecutionError> {
    supervisor.restore_active(symbol).await?;
    let source = if let Err(error) =
        replay_unprocessed_aggregate_trades(supervisor, rest, pool, symbol).await
    {
        tracing::warn!(%error, %symbol, "exact execution replay failed; using OHLC recovery");
        supervisor.restore_active(symbol).await?;
        RecoverySource::OhlcReconciled
    } else {
        RecoverySource::ExactReplay
    };
    reconcile_active_from_m1(supervisor, pool, symbol).await?;
    let checkpoint = supervisor.checkpoint_market_cursor().await?;
    if checkpoint == SupervisorStep::Frozen {
        return Err(ExecutionError::Storage {
            operation: "checkpoint recovered execution cursor",
            message: "execution cursor checkpoint failed after recovery".to_owned(),
        });
    }
    record_execution_recovery(source);
    Ok(())
}

pub(crate) async fn reconcile_active_from_m1(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<(), ExecutionError> {
    let start: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(active_time) FROM ( \
             SELECT actual_entry_time AS active_time \
             FROM virtual_trades \
             WHERE symbol = $1 AND trade_state = 'OPEN' \
               AND NOT (virtual_trade_id = ANY($2)) \
             UNION ALL \
             SELECT (s.definition->>'effectiveAt')::BIGINT AS active_time \
             FROM signals s \
             JOIN tracker_states ts ON ts.signal_id = s.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE s.symbol = $1 \
               AND ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
         ) active",
    )
    .bind(symbol.as_str())
    .bind(supervisor.replaying_trade_ids())
    .fetch_one(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load execution OHLC recovery start",
        message: error.to_string(),
    })?;
    let Some(start) = start else {
        return Ok(());
    };
    let rows = sqlx::query(
        "SELECT open_time, close_time, open, high, low, close \
         FROM candles \
         WHERE symbol = $1 AND interval = '1m' AND open_time >= $2 \
         ORDER BY open_time",
    )
    .bind(symbol.as_str())
    .bind(start)
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load M1 candles for execution recovery",
        message: error.to_string(),
    })?;
    for row in rows {
        let candle = m1_candle(symbol, &row)?;
        let step = supervisor
            .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(candle)))
            .await?;
        if step == SupervisorStep::Frozen {
            return Err(ExecutionError::Storage {
                operation: "reconcile execution from M1 candles",
                message: "execution supervisor froze during OHLC recovery".to_owned(),
            });
        }
    }
    Ok(())
}

/// Brings trades opened from history up to `close_time`. Analysis has finished every earlier
/// bar, so these candles cannot get ahead of the management decisions that apply to them.
/// Returns false when the replay could not run yet and must be tried again.
pub(crate) async fn replay_open_trades_through(
    supervisor: &mut ExecutionSupervisor,
    pool: &PgPool,
    symbol: &Symbol,
    close_time: i64,
) -> Result<bool, ExecutionError> {
    let Some(after) = supervisor.replay_cursor() else {
        return Ok(true);
    };
    if !m1_history_reaches(pool, symbol, after + 1).await? {
        return Ok(false);
    }
    let rows = sqlx::query(
        "SELECT open_time, close_time, open, high, low, close \
         FROM candles \
         WHERE symbol = $1 AND interval = '1m' AND close_time > $2 AND close_time <= $3 \
         ORDER BY open_time",
    )
    .bind(symbol.as_str())
    .bind(after)
    .bind(close_time)
    .fetch_all(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load M1 candles for trade replay",
        message: error.to_string(),
    })?;
    for row in rows {
        let candle = m1_candle(symbol, &row)?;
        let step = supervisor
            .process(SupervisorEvent::ReplayCandle(candle))
            .await?;
        if step == SupervisorStep::Frozen {
            supervisor.restore_active(symbol).await?;
            return Ok(false);
        }
    }
    if let Some(latest) = latest_m1_close_time(pool, symbol).await? {
        supervisor.finish_replay(latest);
    }
    Ok(true)
}

/// Backfill loads M1 history backward, so older candles may still be on their way. Replaying
/// past them would let a trade skip price action it should have seen.
async fn m1_history_reaches(
    pool: &PgPool,
    symbol: &Symbol,
    open_time: i64,
) -> Result<bool, ExecutionError> {
    let state = sqlx::query(
        "SELECT full_history_complete, oldest_synced_time FROM sync_states \
         WHERE symbol = $1 AND interval = '1m'",
    )
    .bind(symbol.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load M1 backfill progress",
        message: error.to_string(),
    })?;
    Ok(state.is_none_or(|state| {
        state.get::<bool, _>("full_history_complete")
            || state
                .get::<Option<i64>, _>("oldest_synced_time")
                .is_some_and(|oldest| oldest <= open_time)
    }))
}

pub(crate) async fn latest_m1_close_time(
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<Option<i64>, ExecutionError> {
    sqlx::query_scalar(
        "SELECT close_time FROM candles \
         WHERE symbol = $1 AND interval = '1m' \
         ORDER BY open_time DESC LIMIT 1",
    )
    .bind(symbol.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load latest M1 close time",
        message: error.to_string(),
    })
}

fn m1_candle(
    symbol: &Symbol,
    row: &sqlx::postgres::PgRow,
) -> Result<market_domain::Candle, ExecutionError> {
    market_domain::Candle::new(
        symbol.clone(),
        Timeframe::M1,
        row.get("open_time"),
        row.get("close_time"),
        row.get("open"),
        row.get("high"),
        row.get("low"),
        row.get("close"),
    )
    .map_err(|error| ExecutionError::InvalidData(error.to_string()))
}

async fn active_market_cursor(
    pool: &PgPool,
    symbol: &Symbol,
) -> Result<Option<i64>, ExecutionError> {
    sqlx::query_scalar(
        "SELECT last_processed_agg_trade_id FROM market_event_cursors WHERE symbol = $1",
    )
    .bind(symbol.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| ExecutionError::Storage {
        operation: "load market event cursor",
        message: error.to_string(),
    })
}
