use crate::{storage_error, storage_sqlx_error};
use analysis_domain::{Direction, PlanFeedback, PlanKind, SignalDefinition, SignalEvent};
use async_trait::async_trait;
use execution_domain::{
    Ambiguity, ExecutionModel, MarketCursor, MonitoringEvent, PositionStage, ReconciliationSource,
    TrackingHealth, TradeHealth, TradeState, VirtualTrade, VirtualTradeEvent,
};
use execution_service::{
    ActiveTradeSequence, DurableTransition, ExecutionError, HealthTransition, TrackerSnapshot,
    TrackerState, TrackerStore,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone)]
pub struct TrackerRepository {
    pool: PgPool,
}

impl TrackerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TrackerStore for TrackerRepository {
    async fn load_active(&self, symbol: &Symbol) -> Result<TrackerSnapshot, ExecutionError> {
        let cursor = sqlx::query(
            "SELECT last_processed_agg_trade_id, last_trade_time \
             FROM market_event_cursors WHERE symbol = $1",
        )
        .bind(symbol.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load market event cursor", error))?
        .map(|row| MarketCursor {
            symbol: symbol.clone(),
            last_processed_agg_trade_id: row.get("last_processed_agg_trade_id"),
            last_trade_time: row.get("last_trade_time"),
        });
        let active_trades = sqlx::query(
            "SELECT * FROM virtual_trades WHERE symbol = $1 AND trade_state = 'OPEN' \
             ORDER BY created_at, virtual_trade_id",
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load active virtual trades", error))?
        .into_iter()
        .map(row_to_trade)
        .collect::<Result<Vec<_>, _>>()?;
        let active_trade_sequences = sqlx::query(
            "SELECT vt.virtual_trade_id, COALESCE(MAX(vte.sequence), 0) + 1 AS next_sequence \
             FROM virtual_trades vt \
             LEFT JOIN virtual_trade_events vte ON vte.virtual_trade_id = vt.virtual_trade_id \
             WHERE vt.symbol = $1 AND vt.trade_state = 'OPEN' \
             GROUP BY vt.virtual_trade_id ORDER BY vt.virtual_trade_id",
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load active virtual trade sequences", error))?
        .into_iter()
        .map(|row| ActiveTradeSequence {
            virtual_trade_id: row.get("virtual_trade_id"),
            next_sequence: row.get("next_sequence"),
        })
        .collect();
        let active_recommendations = sqlx::query(
            "SELECT DISTINCT ON (s.symbol, s.plan, s.interval) s.definition \
             FROM signals s \
             JOIN tracker_states ts ON ts.signal_id = s.signal_id \
             LEFT JOIN virtual_trades vt ON vt.signal_id = s.signal_id \
             WHERE s.symbol = $1 \
               AND ts.state->>'setupState' = 'ENTRY_AVAILABLE' \
               AND ts.state->>'tradeState' = 'NOT_ENTERED' \
               AND vt.signal_id IS NULL \
             ORDER BY s.symbol, s.plan, s.interval, s.created_at DESC, s.signal_id DESC",
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load active recommendations", error))?
        .into_iter()
        .map(|row| {
            serde_json::from_value::<SignalDefinition>(row.get("definition"))
                .map_err(|error| ExecutionError::InvalidData(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
        let tracker_states = sqlx::query(
            "SELECT signal_id, symbol, state, tracking_health \
             FROM tracker_states WHERE symbol = $1 ORDER BY signal_id",
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load tracker states", error))?
        .into_iter()
        .map(row_to_state)
        .collect::<Result<Vec<_>, _>>()?;
        Ok(TrackerSnapshot {
            cursor,
            active_recommendations,
            active_trades,
            active_trade_sequences,
            tracker_states,
        })
    }

    async fn checkpoint_cursor(&self, cursor: MarketCursor) -> Result<(), ExecutionError> {
        sqlx::query(
            "INSERT INTO market_event_cursors \
             (symbol, last_processed_agg_trade_id, last_trade_time) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (symbol) DO UPDATE SET \
                last_processed_agg_trade_id = GREATEST( \
                    market_event_cursors.last_processed_agg_trade_id, \
                    EXCLUDED.last_processed_agg_trade_id \
                ), \
                last_trade_time = CASE \
                    WHEN EXCLUDED.last_processed_agg_trade_id > market_event_cursors.last_processed_agg_trade_id \
                    THEN EXCLUDED.last_trade_time \
                    ELSE market_event_cursors.last_trade_time \
                END, \
                updated_at = CASE \
                    WHEN EXCLUDED.last_processed_agg_trade_id > market_event_cursors.last_processed_agg_trade_id \
                    THEN NOW() \
                    ELSE market_event_cursors.updated_at \
                END",
        )
        .bind(cursor.symbol.as_str())
        .bind(cursor.last_processed_agg_trade_id)
        .bind(cursor.last_trade_time)
        .execute(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("checkpoint market cursor", error))?;
        Ok(())
    }

    async fn commit_transition(
        &self,
        transition: DurableTransition,
    ) -> Result<Option<MonitoringEvent>, ExecutionError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin tracker transition", error))?;
        let supports_management_order = if transition.virtual_trade_write.is_some()
            || !transition.virtual_trade_writes.is_empty()
        {
            supports_management_order(&mut transaction).await?
        } else {
            false
        };
        if let Some(write) = transition.virtual_trade_write {
            insert_trade(&mut transaction, &write.trade, supports_management_order).await?;
        }
        for write in &transition.virtual_trade_writes {
            insert_trade(&mut transaction, &write.trade, supports_management_order).await?;
        }
        for event in &transition.trade_events {
            insert_trade_event(&mut transaction, event).await?;
        }
        for event in &transition.signal_events {
            insert_signal_event(&mut transaction, event).await?;
        }
        for feedback in &transition.plan_feedback {
            insert_plan_feedback(&mut transaction, feedback).await?;
        }
        if let Some(state) = &transition.tracker_state {
            upsert_state(&mut transaction, state).await?;
        }
        for state in &transition.tracker_states {
            upsert_state(&mut transaction, state).await?;
        }
        for directive_id in &transition.consumed_directive_ids {
            sqlx::query("UPDATE trade_management_directives SET consumed_at = NOW() WHERE directive_id = $1")
                .bind(directive_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| storage_sqlx_error("consume directive", error))?;
        }
        if let Some(cursor) = &transition.market_cursor {
            upsert_cursor(&mut transaction, cursor).await?;
        }
        let mut outbox = Vec::new();
        for event in &transition.monitoring_events {
            outbox.push(insert_monitoring_event(&mut transaction, event).await?);
        }
        if transition.inject_failure_after_writes {
            return Err(ExecutionError::Storage {
                operation: "tracker transition failure injection",
                message: "injected rollback".to_owned(),
            });
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit tracker transition", error))?;
        // Write-only transitions (for example MFE/MAE checkpoints) are valid
        // without a monitoring outbox row.
        Ok(outbox.into_iter().next())
    }

    async fn mark_health(
        &self,
        change: HealthTransition,
    ) -> Result<MonitoringEvent, ExecutionError> {
        self.commit_transition(DurableTransition {
            tracker_state: Some(change.state),
            monitoring_events: vec![change.monitoring_event],
            ..DurableTransition::default()
        })
        .await?
        .ok_or_else(|| {
            ExecutionError::InvalidData("health transition requires a monitoring event".to_owned())
        })
    }
}

async fn insert_trade(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    trade: &VirtualTrade,
    supports_management_order: bool,
) -> Result<(), ExecutionError> {
    if !supports_management_order {
        return insert_trade_without_management_order(transaction, trade).await;
    }
    sqlx::query(
        "INSERT INTO virtual_trades \
         (virtual_trade_id, signal_id, symbol, plan, interval, direction, trade_state, \
          actual_entry_time, actual_entry_price, exit_time, exit_price, primary_target, \
          reference_levels, initial_stop, active_stop, position_stage, trade_health, \
          thesis_invalidation, tp1_price, tp1_r, tp1_fraction, tp1_filled_at, \
          remaining_fraction, realized_fraction, realized_r, tp2_zone_low, tp2_zone_high, \
          last_management_effective_at, last_management_candle_time, \
          release_id, algorithm_version, parameter_version, execution_model, \
          reconciliation_source, ambiguity, result, closed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, \
                 $28, $29, $30, $31, $32, $33, $34, $35, $36, \
                 CASE WHEN $7 = 'CLOSED' THEN NOW() ELSE NULL END) \
         ON CONFLICT (virtual_trade_id) DO UPDATE SET \
            trade_state = EXCLUDED.trade_state, \
            exit_time = EXCLUDED.exit_time, \
            exit_price = EXCLUDED.exit_price, \
            primary_target = EXCLUDED.primary_target, \
            active_stop = EXCLUDED.active_stop, \
            position_stage = EXCLUDED.position_stage, \
            trade_health = EXCLUDED.trade_health, \
            thesis_invalidation = EXCLUDED.thesis_invalidation, \
            tp1_price = EXCLUDED.tp1_price, \
            tp1_r = EXCLUDED.tp1_r, \
            tp1_fraction = EXCLUDED.tp1_fraction, \
            tp1_filled_at = EXCLUDED.tp1_filled_at, \
            remaining_fraction = EXCLUDED.remaining_fraction, \
            realized_fraction = EXCLUDED.realized_fraction, \
            realized_r = EXCLUDED.realized_r, \
            tp2_zone_low = EXCLUDED.tp2_zone_low, \
            tp2_zone_high = EXCLUDED.tp2_zone_high, \
            last_management_effective_at = EXCLUDED.last_management_effective_at, \
            last_management_candle_time = EXCLUDED.last_management_candle_time, \
            execution_model = EXCLUDED.execution_model, \
            reconciliation_source = EXCLUDED.reconciliation_source, \
            ambiguity = EXCLUDED.ambiguity, \
            result = EXCLUDED.result, \
            closed_at = CASE WHEN EXCLUDED.trade_state = 'CLOSED' THEN NOW() ELSE virtual_trades.closed_at END",
    )
    .bind(trade.virtual_trade_id)
    .bind(trade.signal_id)
    .bind(trade.symbol.as_str())
    .bind(trade.plan.as_str())
    .bind(trade.timeframe.as_str())
    .bind(direction_text(trade.direction))
    .bind(trade_state_text(trade.trade_state))
    .bind(trade.actual_entry_time)
    .bind(trade.actual_entry_price)
    .bind(trade.exit_time)
    .bind(trade.exit_price)
    .bind(trade.primary_target)
    .bind(
        serde_json::to_value(&trade.reference_levels)
            .map_err(|error| storage_error("serialize reference levels", error))?,
    )
    .bind(trade.initial_stop)
    .bind(trade.active_stop)
    .bind(position_stage_text(trade.position_stage))
    .bind(trade_health_text(trade.trade_health))
    .bind(trade.thesis_invalidation)
    .bind(trade.tp1_price)
    .bind(trade.tp1_r)
    .bind(trade.tp1_fraction)
    .bind(trade.tp1_filled_at)
    .bind(trade.remaining_fraction)
    .bind(trade.realized_fraction)
    .bind(trade.realized_r)
    .bind(trade.tp2_zone_low)
    .bind(trade.tp2_zone_high)
    .bind(trade.last_management_effective_at)
    .bind(trade.last_management_candle_time)
    .bind(trade.versions.release_id)
    .bind(&trade.versions.algorithm_version)
    .bind(&trade.versions.parameter_version)
    .bind(execution_model_text(trade.execution_model))
    .bind(trade.reconciliation_source.map(reconciliation_text))
    .bind(ambiguity_text(trade.ambiguity))
    .bind(
        serde_json::to_value(serde_json::json!({
            "grossR": trade.gross_r,
            "mfeR": trade.mfe_r,
            "maeR": trade.mae_r,
            "durationMs": trade.duration_ms,
            "exitReason": trade.exit_reason,
        }))
        .map_err(|error| storage_error("serialize trade result", error))?,
    )
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert virtual trade", error))?;
    Ok(())
}

async fn supports_management_order(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<bool, ExecutionError> {
    sqlx::query_scalar(
        "SELECT COUNT(*) = 2 FROM information_schema.columns \
         WHERE table_schema = current_schema() \
           AND table_name = 'virtual_trades' \
           AND column_name IN ('last_management_effective_at', 'last_management_candle_time')",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("inspect virtual trade management columns", error))
}

async fn insert_trade_without_management_order(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    trade: &VirtualTrade,
) -> Result<(), ExecutionError> {
    sqlx::query(
        "INSERT INTO virtual_trades \
         (virtual_trade_id, signal_id, symbol, plan, interval, direction, trade_state, \
          actual_entry_time, actual_entry_price, exit_time, exit_price, primary_target, \
          reference_levels, initial_stop, active_stop, position_stage, trade_health, \
          thesis_invalidation, tp1_price, tp1_r, tp1_fraction, tp1_filled_at, \
          remaining_fraction, realized_fraction, realized_r, tp2_zone_low, tp2_zone_high, \
          release_id, algorithm_version, parameter_version, execution_model, \
          reconciliation_source, ambiguity, result, closed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, \
                 $28, $29, $30, $31, $32, $33, $34, \
                 CASE WHEN $7 = 'CLOSED' THEN NOW() ELSE NULL END) \
         ON CONFLICT (virtual_trade_id) DO UPDATE SET \
            trade_state = EXCLUDED.trade_state, \
            exit_time = EXCLUDED.exit_time, \
            exit_price = EXCLUDED.exit_price, \
            primary_target = EXCLUDED.primary_target, \
            active_stop = EXCLUDED.active_stop, \
            position_stage = EXCLUDED.position_stage, \
            trade_health = EXCLUDED.trade_health, \
            thesis_invalidation = EXCLUDED.thesis_invalidation, \
            tp1_price = EXCLUDED.tp1_price, \
            tp1_r = EXCLUDED.tp1_r, \
            tp1_fraction = EXCLUDED.tp1_fraction, \
            tp1_filled_at = EXCLUDED.tp1_filled_at, \
            remaining_fraction = EXCLUDED.remaining_fraction, \
            realized_fraction = EXCLUDED.realized_fraction, \
            realized_r = EXCLUDED.realized_r, \
            tp2_zone_low = EXCLUDED.tp2_zone_low, \
            tp2_zone_high = EXCLUDED.tp2_zone_high, \
            execution_model = EXCLUDED.execution_model, \
            reconciliation_source = EXCLUDED.reconciliation_source, \
            ambiguity = EXCLUDED.ambiguity, \
            result = EXCLUDED.result, \
            closed_at = CASE WHEN EXCLUDED.trade_state = 'CLOSED' THEN NOW() ELSE virtual_trades.closed_at END",
    )
    .bind(trade.virtual_trade_id)
    .bind(trade.signal_id)
    .bind(trade.symbol.as_str())
    .bind(trade.plan.as_str())
    .bind(trade.timeframe.as_str())
    .bind(direction_text(trade.direction))
    .bind(trade_state_text(trade.trade_state))
    .bind(trade.actual_entry_time)
    .bind(trade.actual_entry_price)
    .bind(trade.exit_time)
    .bind(trade.exit_price)
    .bind(trade.primary_target)
    .bind(
        serde_json::to_value(&trade.reference_levels)
            .map_err(|error| storage_error("serialize reference levels", error))?,
    )
    .bind(trade.initial_stop)
    .bind(trade.active_stop)
    .bind(position_stage_text(trade.position_stage))
    .bind(trade_health_text(trade.trade_health))
    .bind(trade.thesis_invalidation)
    .bind(trade.tp1_price)
    .bind(trade.tp1_r)
    .bind(trade.tp1_fraction)
    .bind(trade.tp1_filled_at)
    .bind(trade.remaining_fraction)
    .bind(trade.realized_fraction)
    .bind(trade.realized_r)
    .bind(trade.tp2_zone_low)
    .bind(trade.tp2_zone_high)
    .bind(trade.versions.release_id)
    .bind(&trade.versions.algorithm_version)
    .bind(&trade.versions.parameter_version)
    .bind(execution_model_text(trade.execution_model))
    .bind(trade.reconciliation_source.map(reconciliation_text))
    .bind(ambiguity_text(trade.ambiguity))
    .bind(
        serde_json::to_value(serde_json::json!({
            "grossR": trade.gross_r,
            "mfeR": trade.mfe_r,
            "maeR": trade.mae_r,
            "durationMs": trade.duration_ms,
            "exitReason": trade.exit_reason,
        }))
        .map_err(|error| storage_error("serialize trade result", error))?,
    )
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert legacy virtual trade", error))?;
    Ok(())
}

async fn insert_trade_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &VirtualTradeEvent,
) -> Result<(), ExecutionError> {
    sqlx::query(
        "INSERT INTO virtual_trade_events \
         (event_id, virtual_trade_id, sequence, event_type, event_time, payload) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(event.event_id)
    .bind(event.virtual_trade_id)
    .bind(event.sequence)
    .bind(event.kind.as_str())
    .bind(event.event_time)
    .bind(&event.payload)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert virtual trade event", error))?;
    Ok(())
}

async fn insert_signal_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &SignalEvent,
) -> Result<(), ExecutionError> {
    sqlx::query(
        "INSERT INTO signal_events \
         (event_id, signal_id, sequence, event_type, payload, effective_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (signal_id, sequence) DO NOTHING",
    )
    .bind(event.event_id)
    .bind(event.signal_id)
    .bind(event.sequence)
    .bind(event.kind.as_str())
    .bind(
        serde_json::to_value(event)
            .map_err(|error| storage_error("serialize signal event", error))?,
    )
    .bind(event.effective_at)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert signal event", error))?;
    Ok(())
}

async fn insert_plan_feedback(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    feedback: &PlanFeedback,
) -> Result<(), ExecutionError> {
    let (signal_id, virtual_trade_id, feedback_type) = feedback_identity(feedback);
    sqlx::query(
        "INSERT INTO plan_feedback \
         (feedback_id, signal_id, virtual_trade_id, feedback_type, payload) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (signal_id, virtual_trade_id, feedback_type) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(signal_id)
    .bind(virtual_trade_id)
    .bind(feedback_type)
    .bind(
        serde_json::to_value(feedback)
            .map_err(|error| storage_error("serialize plan feedback", error))?,
    )
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert plan feedback", error))?;
    Ok(())
}

async fn upsert_state(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    state: &TrackerState,
) -> Result<(), ExecutionError> {
    sqlx::query(
        "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (signal_id) DO UPDATE SET \
            state = EXCLUDED.state, tracking_health = EXCLUDED.tracking_health, updated_at = NOW()",
    )
    .bind(state.signal_id)
    .bind(state.symbol.as_str())
    .bind(&state.state)
    .bind(health_text(state.tracking_health))
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("upsert tracker state", error))?;
    Ok(())
}

async fn upsert_cursor(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cursor: &MarketCursor,
) -> Result<(), ExecutionError> {
    sqlx::query(
        "INSERT INTO market_event_cursors \
         (symbol, last_processed_agg_trade_id, last_trade_time) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (symbol) DO UPDATE SET \
            last_processed_agg_trade_id = GREATEST( \
                market_event_cursors.last_processed_agg_trade_id, \
                EXCLUDED.last_processed_agg_trade_id \
            ), \
            last_trade_time = CASE \
                WHEN EXCLUDED.last_processed_agg_trade_id > market_event_cursors.last_processed_agg_trade_id \
                THEN EXCLUDED.last_trade_time \
                ELSE market_event_cursors.last_trade_time \
            END, \
            updated_at = CASE \
                WHEN EXCLUDED.last_processed_agg_trade_id > market_event_cursors.last_processed_agg_trade_id \
                THEN NOW() \
                ELSE market_event_cursors.updated_at \
            END",
    )
    .bind(cursor.symbol.as_str())
    .bind(cursor.last_processed_agg_trade_id)
    .bind(cursor.last_trade_time)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("upsert market cursor", error))?;
    Ok(())
}

async fn insert_monitoring_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &execution_service::NewMonitoringEvent,
) -> Result<MonitoringEvent, ExecutionError> {
    let row = sqlx::query(
        "INSERT INTO monitoring_events \
         (event_id, event_type, entity_type, entity_id, payload) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING cursor, created_at",
    )
    .bind(event.event_id)
    .bind(&event.event_type)
    .bind(&event.entity_type)
    .bind(event.entity_id)
    .bind(&event.payload)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage_sqlx_error("insert monitoring event", error))?;
    Ok(MonitoringEvent {
        cursor: row.get("cursor"),
        event_id: event.event_id,
        event_type: event.event_type.clone(),
        entity_type: event.entity_type.clone(),
        entity_id: event.entity_id,
        payload: event.payload.clone(),
        created_at: row
            .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
            .timestamp_millis(),
    })
}

fn row_to_state(row: sqlx::postgres::PgRow) -> Result<TrackerState, ExecutionError> {
    let symbol: String = row.get("symbol");
    let health: String = row.get("tracking_health");
    Ok(TrackerState {
        signal_id: row.get("signal_id"),
        symbol: Symbol::new(symbol)
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
        state: row.get("state"),
        tracking_health: parse_health(&health)?,
    })
}

pub(crate) fn row_to_trade(row: sqlx::postgres::PgRow) -> Result<VirtualTrade, ExecutionError> {
    let symbol: String = row.get("symbol");
    let plan: String = row.get("plan");
    let interval: String = row.get("interval");
    let direction: String = row.get("direction");
    let state: String = row.get("trade_state");
    let position_stage: String = row.get("position_stage");
    let trade_health: String = row.get("trade_health");
    let execution_model: String = row.get("execution_model");
    let ambiguity: String = row.get("ambiguity");
    let reconciliation: Option<String> = row.get("reconciliation_source");
    let result: Option<serde_json::Value> = row.get("result");
    Ok(VirtualTrade {
        virtual_trade_id: row.get("virtual_trade_id"),
        signal_id: row.get("signal_id"),
        symbol: Symbol::new(symbol)
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
        plan: parse_plan(&plan)?,
        timeframe: Timeframe::from_str(&interval)
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
        direction: parse_direction(&direction)?,
        trade_state: parse_trade_state(&state)?,
        position_stage: parse_position_stage(&position_stage)?,
        trade_health: parse_trade_health(&trade_health)?,
        actual_entry_time: row.get("actual_entry_time"),
        actual_entry_price: row.get("actual_entry_price"),
        exit_time: row.get("exit_time"),
        exit_price: row.get("exit_price"),
        primary_target: row.get("primary_target"),
        reference_levels: serde_json::from_value(row.get("reference_levels"))
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?,
        initial_stop: row.get("initial_stop"),
        active_stop: row.get("active_stop"),
        thesis_invalidation: row.get("thesis_invalidation"),
        tp1_price: row.get("tp1_price"),
        tp1_r: row.get("tp1_r"),
        tp1_fraction: row.get("tp1_fraction"),
        tp1_filled_at: row.get("tp1_filled_at"),
        remaining_fraction: row.get("remaining_fraction"),
        realized_fraction: row.get("realized_fraction"),
        realized_r: row.get("realized_r"),
        tp2_zone_low: row.get("tp2_zone_low"),
        tp2_zone_high: row.get("tp2_zone_high"),
        last_management_effective_at: optional_i64(&row, "last_management_effective_at")?,
        last_management_candle_time: optional_i64(&row, "last_management_candle_time")?,
        versions: analysis_domain::VersionIdentity {
            release_id: row.get("release_id"),
            algorithm_version: row.get("algorithm_version"),
            parameter_version: row.get("parameter_version"),
        },
        execution_model: parse_execution_model(&execution_model)?,
        reconciliation_source: reconciliation
            .as_deref()
            .map(parse_reconciliation)
            .transpose()?,
        ambiguity: parse_ambiguity(&ambiguity)?,
        gross_r: result
            .as_ref()
            .and_then(|value| value.get("grossR"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?
            .flatten(),
        mfe_r: result_decimal(&result, "mfeR").unwrap_or(Decimal::ZERO),
        mae_r: result_decimal(&result, "maeR").unwrap_or(Decimal::ZERO),
        duration_ms: result
            .as_ref()
            .and_then(|value| value.get("durationMs"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?
            .flatten(),
        exit_reason: result
            .as_ref()
            .and_then(|value| value.get("exitReason"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| ExecutionError::InvalidData(error.to_string()))?
            .flatten(),
    })
}

fn optional_i64(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
) -> Result<Option<i64>, ExecutionError> {
    match row.try_get(column) {
        Ok(value) => Ok(value),
        Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
        Err(error) => Err(storage_error("decode optional virtual trade column", error)),
    }
}

fn result_decimal(result: &Option<serde_json::Value>, key: &str) -> Option<Decimal> {
    result
        .as_ref()
        .and_then(|value| value.get(key))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .flatten()
}

fn feedback_identity(feedback: &PlanFeedback) -> (Uuid, Uuid, &'static str) {
    match feedback {
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "ACTUAL_ENTRY"),
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "PARTIAL_TARGET_FILLED"),
        PlanFeedback::TradeClosed {
            signal_id,
            virtual_trade_id,
            ..
        } => (*signal_id, *virtual_trade_id, "TRADE_CLOSED"),
    }
}

fn direction_text(value: Direction) -> &'static str {
    match value {
        Direction::Buy => "BUY",
        Direction::Sell => "SELL",
        Direction::None => "NONE",
    }
}

fn parse_direction(value: &str) -> Result<Direction, ExecutionError> {
    match value {
        "BUY" => Ok(Direction::Buy),
        "SELL" => Ok(Direction::Sell),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown direction {other}"
        ))),
    }
}

fn trade_state_text(value: TradeState) -> &'static str {
    match value {
        TradeState::NotEntered => "NOT_ENTERED",
        TradeState::Open => "OPEN",
        TradeState::Closed => "CLOSED",
    }
}

fn position_stage_text(value: PositionStage) -> &'static str {
    match value {
        PositionStage::Legacy => "LEGACY",
        PositionStage::FullPosition => "FULL_POSITION",
        PositionStage::Runner => "RUNNER",
    }
}

fn parse_position_stage(value: &str) -> Result<PositionStage, ExecutionError> {
    match value {
        "LEGACY" => Ok(PositionStage::Legacy),
        "FULL_POSITION" => Ok(PositionStage::FullPosition),
        "RUNNER" => Ok(PositionStage::Runner),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown position stage {other}"
        ))),
    }
}

fn trade_health_text(value: TradeHealth) -> &'static str {
    match value {
        TradeHealth::Healthy => "HEALTHY",
        TradeHealth::Weakening => "WEAKENING",
        TradeHealth::Invalid => "INVALID",
    }
}

fn parse_trade_health(value: &str) -> Result<TradeHealth, ExecutionError> {
    match value {
        "HEALTHY" => Ok(TradeHealth::Healthy),
        "WEAKENING" => Ok(TradeHealth::Weakening),
        "INVALID" => Ok(TradeHealth::Invalid),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown trade health {other}"
        ))),
    }
}

fn parse_trade_state(value: &str) -> Result<TradeState, ExecutionError> {
    match value {
        "OPEN" => Ok(TradeState::Open),
        "CLOSED" => Ok(TradeState::Closed),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown trade state {other}"
        ))),
    }
}

fn execution_model_text(value: ExecutionModel) -> &'static str {
    match value {
        ExecutionModel::LiveTrade => "LIVE_TRADE",
        ExecutionModel::OhlcConservative => "OHLC_CONSERVATIVE",
    }
}

fn parse_execution_model(value: &str) -> Result<ExecutionModel, ExecutionError> {
    match value {
        "LIVE_TRADE" => Ok(ExecutionModel::LiveTrade),
        "OHLC_CONSERVATIVE" => Ok(ExecutionModel::OhlcConservative),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown execution model {other}"
        ))),
    }
}

fn reconciliation_text(value: ReconciliationSource) -> &'static str {
    match value {
        ReconciliationSource::ExactTrade => "EXACT_TRADE",
        ReconciliationSource::OhlcReconciled => "OHLC_RECONCILED",
    }
}

fn parse_reconciliation(value: &str) -> Result<ReconciliationSource, ExecutionError> {
    match value {
        "EXACT_TRADE" => Ok(ReconciliationSource::ExactTrade),
        "OHLC_RECONCILED" => Ok(ReconciliationSource::OhlcReconciled),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown reconciliation {other}"
        ))),
    }
}

fn ambiguity_text(value: Ambiguity) -> &'static str {
    match value {
        Ambiguity::None => "NONE",
        Ambiguity::Ambiguous => "AMBIGUOUS",
    }
}

fn parse_ambiguity(value: &str) -> Result<Ambiguity, ExecutionError> {
    match value {
        "NONE" => Ok(Ambiguity::None),
        "AMBIGUOUS" => Ok(Ambiguity::Ambiguous),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown ambiguity {other}"
        ))),
    }
}

fn health_text(value: TrackingHealth) -> &'static str {
    match value {
        TrackingHealth::Live => "LIVE",
        TrackingHealth::Stale => "STALE",
        TrackingHealth::Reconciling => "RECONCILING",
        TrackingHealth::MarketClosed => "MARKET_CLOSED",
    }
}

fn parse_health(value: &str) -> Result<TrackingHealth, ExecutionError> {
    match value {
        "LIVE" => Ok(TrackingHealth::Live),
        "STALE" => Ok(TrackingHealth::Stale),
        "RECONCILING" => Ok(TrackingHealth::Reconciling),
        "MARKET_CLOSED" => Ok(TrackingHealth::MarketClosed),
        other => Err(ExecutionError::InvalidData(format!(
            "unknown health {other}"
        ))),
    }
}

fn parse_plan(value: &str) -> Result<PlanKind, ExecutionError> {
    match value {
        "ICT" => Ok(PlanKind::Ict),
        "SMC" => Ok(PlanKind::Smc),
        "RSI" => Ok(PlanKind::Rsi),
        "QUASIMODO" => Ok(PlanKind::Quasimodo),
        "TREND_BREAKOUT" => Ok(PlanKind::TrendBreakout),
        "MEAN_REVERSION" => Ok(PlanKind::MeanReversion),
        other => Err(ExecutionError::InvalidData(format!("unknown plan {other}"))),
    }
}
