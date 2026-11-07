use super::execution::refresh_market_closures;
use super::recovery::replay_open_trades_through;
use super::*;
use analysis_domain::{ManagementDirective, SignalDefinition};
use execution_service::{ExecutionSupervisor, SupervisorEvent, SupervisorStep};
use market_domain::MarketEvent;

#[cfg(test)]
mod runtime_repair_tests {
    use super::*;
    use analysis_domain::{Direction, DirectiveKind, ExecutionInstruction, PriceZone};
    use std::sync::Mutex;
    use tokio::sync::OwnedMutexGuard;

    const NOW_MS: i64 = 1_782_195_600_000;

    async fn test_pool() -> (PgPool, OwnedMutexGuard<()>) {
        let guard = TEST_DB_LOCK
            .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await;
        dotenvy::dotenv().ok();
        let database_url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL must point to an isolated test database");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("connect to TEST_DATABASE_URL");
        MIGRATOR.run(&pool).await.expect("run migrations");
        sqlx::query(
            "TRUNCATE backtest_trades, backtest_runs, monitoring_events, \
             virtual_trade_events, virtual_trades, tracker_states, market_event_cursors, plan_feedback, \
             rejected_plan_candidates, trade_management_directives, signal_events, signals, \
             analysis_evaluations, plan_runtime_states, analysis_work_items, \
             analysis_runtime_state, analysis_profiles, configuration_releases, \
             market_sessions, sync_states, candles, markets RESTART IDENTITY CASCADE",
        )
        .execute(&pool)
        .await
        .expect("truncate runtime repair tables");
        sqlx::query(
            "INSERT INTO markets \
             (symbol, enabled, status, price_tick, onboard_time, session_market) \
             VALUES ('XAUUSDT', TRUE, 'TRADING', 0.01, 0, 'COMMODITY')",
        )
        .execute(&pool)
        .await
        .expect("seed market");
        (pool, guard)
    }

    fn test_symbol() -> Symbol {
        Symbol::new("XAUUSDT").unwrap()
    }

    #[derive(Clone)]
    struct PagedTradeSource {
        max_id: i64,
        calls: Arc<Mutex<Vec<i64>>>,
    }

    #[async_trait]
    impl market_service::MarketSource for PagedTradeSource {
        async fn server_time(&self) -> Result<i64, ServiceError> {
            panic!("server_time is not used by execution replay")
        }

        async fn market(&self, _symbol: &Symbol) -> Result<market_domain::Market, ServiceError> {
            panic!("market is not used by execution replay")
        }

        async fn klines(
            &self,
            _symbol: &Symbol,
            _timeframe: Timeframe,
            _start_time: Option<i64>,
            _end_time: Option<i64>,
            _limit: u16,
        ) -> Result<Vec<market_domain::Candle>, ServiceError> {
            panic!("klines are not used by exact execution replay")
        }

        async fn aggregate_trades_from(
            &self,
            symbol: &Symbol,
            from_id: i64,
            limit: u16,
        ) -> Result<Vec<market_domain::AggregateTrade>, ServiceError> {
            self.calls.lock().unwrap().push(from_id);
            if from_id > self.max_id {
                return Ok(Vec::new());
            }
            let end = (from_id + i64::from(limit) - 1).min(self.max_id);
            Ok((from_id..=end)
                .map(|id| {
                    market_domain::AggregateTrade::new(
                        symbol.clone(),
                        id,
                        id,
                        id,
                        id * 10,
                        id * 10,
                        Decimal::new(4100, 0),
                    )
                    .unwrap()
                })
                .collect())
        }

        async fn trading_schedule(
            &self,
            _session_market: &str,
        ) -> Result<Vec<market_domain::MarketSession>, ServiceError> {
            panic!("trading_schedule is not used by execution replay")
        }
    }

    fn test_signal(signal_id: uuid::Uuid, release_id: uuid::Uuid) -> SignalDefinition {
        SignalDefinition {
            signal_id,
            symbol: test_symbol(),
            plan: PlanKind::Ict,
            target_timeframe: Timeframe::M1,
            evaluated_at: NOW_MS,
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
            effective_at: NOW_MS,
            detected_at: NOW_MS,
            published_at: NOW_MS,
            release_id,
            algorithm_version: "ict-v1".to_owned(),
            parameter_version: "ict-1m-v1".to_owned(),
            created_at: NOW_MS,
            expires_at: None,
            htf_context: None,
        }
    }

    async fn seed_signal(pool: &PgPool, signal: &SignalDefinition) {
        sqlx::query(
            "INSERT INTO signals \
             (signal_id, symbol, plan, interval, definition, evidence, release_id, \
              algorithm_version, parameter_version, created_at) \
             VALUES ($1, $2, $3, $4, $5, '[]', $6, $7, $8, to_timestamp($9 / 1000.0))",
        )
        .bind(signal.signal_id)
        .bind(signal.symbol.as_str())
        .bind(signal.plan.as_str())
        .bind(signal.target_timeframe.as_str())
        .bind(serde_json::to_value(signal).unwrap())
        .bind(signal.release_id)
        .bind(&signal.algorithm_version)
        .bind(&signal.parameter_version)
        .bind(signal.created_at)
        .execute(pool)
        .await
        .expect("seed signal");
        sqlx::query(
            "INSERT INTO signal_events (event_id, signal_id, sequence, event_type, payload, effective_at) \
             VALUES ($1, $2, 0, 'ENTRY_AVAILABLE', $3, $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(signal.signal_id)
        .bind(json!({
            "eventId": uuid::Uuid::now_v7(),
            "signalId": signal.signal_id,
            "sequence": 0,
            "kind": "ENTRY_AVAILABLE",
            "effectiveAt": signal.effective_at,
            "recordedAt": signal.detected_at,
            "virtualTradeId": null,
            "price": null,
            "reason": null
        }))
        .bind(signal.effective_at)
        .execute(pool)
        .await
        .expect("seed entry event");
        sqlx::query(
            "INSERT INTO tracker_states (signal_id, symbol, state, tracking_health) \
             VALUES ($1, 'XAUUSDT', $2, 'LIVE')",
        )
        .bind(signal.signal_id)
        .bind(json!({
            "signalId": signal.signal_id,
            "setupState": "ENTRY_AVAILABLE",
            "tradeState": "NOT_ENTERED"
        }))
        .execute(pool)
        .await
        .expect("seed tracker state");
    }

    async fn seed_pending_signal(pool: &PgPool, signal: &SignalDefinition) {
        sqlx::query(
            "INSERT INTO signals \
             (signal_id, symbol, plan, interval, definition, evidence, release_id, \
              algorithm_version, parameter_version, created_at) \
             VALUES ($1, $2, $3, $4, $5, '[]', $6, $7, $8, to_timestamp($9 / 1000.0))",
        )
        .bind(signal.signal_id)
        .bind(signal.symbol.as_str())
        .bind(signal.plan.as_str())
        .bind(signal.target_timeframe.as_str())
        .bind(serde_json::to_value(signal).unwrap())
        .bind(signal.release_id)
        .bind(&signal.algorithm_version)
        .bind(&signal.parameter_version)
        .bind(signal.created_at)
        .execute(pool)
        .await
        .expect("seed pending signal");
    }

    async fn seed_candle(
        pool: &PgPool,
        timeframe: Timeframe,
        open_time: i64,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
    ) {
        let candle = market_domain::Candle::new(
            test_symbol(),
            timeframe,
            open_time,
            open_time + timeframe.duration_ms() - 1,
            open,
            high,
            low,
            close,
        )
        .unwrap();
        CandleRepository::new(pool.clone())
            .upsert_page(&[candle])
            .await
            .expect("seed candle");
    }

    async fn seed_latest_candle(pool: &PgPool) {
        let candle = market_domain::Candle::new(
            test_symbol(),
            Timeframe::M1,
            NOW_MS,
            NOW_MS + Timeframe::M1.duration_ms() - 1,
            Decimal::new(4110, 0),
            Decimal::new(4140, 0),
            Decimal::new(4100, 0),
            Decimal::new(4135, 0),
        )
        .unwrap();
        CandleRepository::new(pool.clone())
            .upsert_page(&[candle])
            .await
            .expect("seed latest candle");
    }

    async fn seed_regular_session(pool: &PgPool, start: i64, end: i64) {
        sqlx::query(
            "INSERT INTO market_sessions \
             (session_market, session_status, session_start_time, session_end_time, source_event_time) \
             VALUES ('COMMODITY', 'REGULAR', $1, $2, $3)",
        )
        .bind(start)
        .bind(end)
        .bind(NOW_MS)
        .execute(pool)
        .await
        .expect("seed regular session");
    }

    async fn seed_gap_candles(pool: &PgPool) {
        let candles = [0_i64, 60_000, 180_000]
            .into_iter()
            .map(|open_time| {
                market_domain::Candle::new(
                    test_symbol(),
                    Timeframe::M1,
                    open_time,
                    open_time + Timeframe::M1.duration_ms() - 1,
                    Decimal::new(4100, 0),
                    Decimal::new(4110, 0),
                    Decimal::new(4090, 0),
                    Decimal::new(4105, 0),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        CandleRepository::new(pool.clone())
            .upsert_page(&candles)
            .await
            .expect("seed gap candles");
    }

    #[tokio::test]
    async fn terminal_recommendation_repair_publishes_terminal_events_for_stream_consumers() {
        let (pool, _guard) = test_pool().await;
        let signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        seed_signal(&pool, &signal).await;
        seed_latest_candle(&pool).await;

        repair_terminal_recommendation_states_at_latest_close(
            &pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            None,
        )
        .await
        .unwrap();

        let state: Value =
            sqlx::query_scalar("SELECT state FROM tracker_states WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state["setupState"], "INVALIDATED");
        assert_eq!(state["reason"], "target touched before entry");

        let signal_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM signal_events \
             WHERE signal_id = $1 AND sequence = 1 AND event_type = 'INVALIDATED'",
        )
        .bind(signal.signal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(signal_event_count, 1);

        let monitoring_payload: Value = sqlx::query_scalar(
            "SELECT payload FROM monitoring_events \
             WHERE event_type = 'RECOMMENDATION_INVALIDATED' AND entity_id = $1",
        )
        .bind(signal.signal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(monitoring_payload["signalId"], signal.signal_id.to_string());
        assert_eq!(monitoring_payload["plan"], "ICT");
        assert_eq!(monitoring_payload["timeframe"], "1m");
        assert_eq!(monitoring_payload["direction"], "BUY");
        assert_eq!(monitoring_payload["condition"], "INVALIDATED");
        assert_eq!(monitoring_payload["setupState"], "INVALIDATED");
        assert_eq!(monitoring_payload["tradeState"], "NOT_ENTERED");
        assert_eq!(monitoring_payload["reason"], "target touched before entry");
        pool.close().await;
    }

    #[tokio::test]
    async fn terminal_recommendation_repair_ignores_signals_not_effective_at_latest_close() {
        let (pool, _guard) = test_pool().await;
        let mut signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        signal.effective_at = NOW_MS + Timeframe::M1.duration_ms();
        seed_signal(&pool, &signal).await;
        seed_latest_candle(&pool).await;

        repair_terminal_recommendation_states_at_latest_close(
            &pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            None,
        )
        .await
        .unwrap();

        let state: Value =
            sqlx::query_scalar("SELECT state FROM tracker_states WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state["setupState"], "ENTRY_AVAILABLE");

        let terminal_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM monitoring_events \
             WHERE entity_id = $1 \
               AND event_type IN ('RECOMMENDATION_INVALIDATED', 'RECOMMENDATION_EXPIRED')",
        )
        .bind(signal.signal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(terminal_event_count, 0);
        pool.close().await;
    }

    #[tokio::test]
    async fn pending_next_open_signal_replays_stored_canonical_open() {
        let (pool, _guard) = test_pool().await;
        let mut signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        signal.plan = PlanKind::TrendBreakout;
        signal.target_timeframe = Timeframe::M5;
        signal.execution_instruction = ExecutionInstruction::NextOpen;
        signal.entry_zone = None;
        signal.projected_entry = Some(Decimal::new(417_849, 2));
        signal.stop_loss = Decimal::new(417_702, 2);
        signal.primary_target = Decimal::new(418_069, 2);
        signal.parameter_version = "trend-5m-v1".to_owned();
        seed_pending_signal(&pool, &signal).await;
        seed_candle(
            &pool,
            Timeframe::M5,
            signal.effective_at,
            Decimal::new(417_849, 2),
            Decimal::new(417_899, 2),
            Decimal::new(417_849, 2),
            Decimal::new(417_882, 2),
        )
        .await;

        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        process_pending_execution_inputs(
            &mut supervisor,
            &pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            &mut HashSet::new(),
            &mut HashSet::new(),
            None,
        )
        .await
        .unwrap();

        let trade_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM virtual_trades WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(trade_count, 1);

        let entry_price: Decimal = sqlx::query_scalar(
            "SELECT actual_entry_price FROM virtual_trades WHERE signal_id = $1",
        )
        .bind(signal.signal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entry_price, Decimal::new(417_849, 2));
        pool.close().await;
    }

    const M1_MS: i64 = 60_000;
    const M5_MS: i64 = 300_000;

    /// Regular up to the signal's next open, closed for two M5 bars, then regular again.
    async fn seed_closure_after_signal(pool: &PgPool, signal: &SignalDefinition) -> i64 {
        let closure_start = signal.effective_at;
        let reopening = closure_start + 2 * M5_MS;
        for (status, start, end) in [
            ("REGULAR", closure_start - M5_MS, closure_start),
            ("NO_TRADING", closure_start, reopening),
            ("REGULAR", reopening, reopening + 2 * M5_MS),
        ] {
            sqlx::query(
                "INSERT INTO market_sessions \
                 (session_market, session_status, session_start_time, session_end_time, source_event_time) \
                 VALUES ('COMMODITY', $1, $2, $3, $4)",
            )
            .bind(status)
            .bind(start)
            .bind(end)
            .bind(NOW_MS)
            .execute(pool)
            .await
            .expect("seed session");
        }
        reopening
    }

    fn m5_candle(open_time: i64, open: i64) -> market_domain::Candle {
        market_domain::Candle::new(
            test_symbol(),
            Timeframe::M5,
            open_time,
            open_time + M5_MS - 1,
            Decimal::new(open, 0),
            Decimal::new(open + 1, 0),
            Decimal::new(open - 1, 0),
            Decimal::new(open, 0),
        )
        .unwrap()
    }

    async fn trade_entry(pool: &PgPool, signal_id: uuid::Uuid) -> Option<(i64, Decimal)> {
        sqlx::query_as(
            "SELECT actual_entry_time, actual_entry_price FROM virtual_trades WHERE signal_id = $1",
        )
        .bind(signal_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    async fn deliver_pending_signals(supervisor: &mut ExecutionSupervisor, pool: &PgPool) {
        process_pending_execution_inputs(
            supervisor,
            pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            &mut HashSet::new(),
            &mut HashSet::new(),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stored_next_open_before_a_closure_fills_at_the_first_tradable_bar() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        seed_pending_signal(&pool, &signal).await;
        let reopening = seed_closure_after_signal(&pool, &signal).await;
        let repository = CandleRepository::new(pool.clone());
        repository
            .upsert_page(&[
                m5_candle(signal.effective_at, 4_100),
                m5_candle(reopening, 4_105),
            ])
            .await
            .unwrap();

        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        deliver_pending_signals(&mut supervisor, &pool).await;

        assert_eq!(
            trade_entry(&pool, signal.signal_id).await,
            Some((reopening, Decimal::new(4_105, 0)))
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn waiting_next_open_ignores_closed_bars_across_restore_and_restart() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        seed_pending_signal(&pool, &signal).await;
        let reopening = seed_closure_after_signal(&pool, &signal).await;
        let closed_bar = m5_candle(signal.effective_at, 4_100);
        CandleRepository::new(pool.clone())
            .upsert_page(std::slice::from_ref(&closed_bar))
            .await
            .unwrap();

        let tracker = Arc::new(TrackerRepository::new(pool.clone()));
        let mut supervisor = ExecutionSupervisor::new(tracker.clone());
        supervisor.restore_active(&test_symbol()).await.unwrap();
        deliver_pending_signals(&mut supervisor, &pool).await;
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        // A recovery restores the tracker in place and the closed bar arrives from the stream.
        supervisor.restore_active(&test_symbol()).await.unwrap();
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(
                closed_bar.clone(),
            )))
            .await
            .unwrap();
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        // A process restart builds the wait again from the stored signal.
        let mut restarted = ExecutionSupervisor::new(tracker);
        restarted.restore_active(&test_symbol()).await.unwrap();
        process_active_next_open_canonical_opens(&mut restarted, &pool)
            .await
            .unwrap();
        restarted
            .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(
                closed_bar,
            )))
            .await
            .unwrap();
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        restarted
            .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(
                m5_candle(reopening, 4_105),
            )))
            .await
            .unwrap();
        assert_eq!(
            trade_entry(&pool, signal.signal_id).await,
            Some((reopening, Decimal::new(4_105, 0)))
        );
        pool.close().await;
    }

    async fn seed_no_trading_session(pool: &PgPool, start: i64, end: i64) {
        sqlx::query(
            "INSERT INTO market_sessions \
             (session_market, session_status, session_start_time, session_end_time, source_event_time) \
             VALUES ('COMMODITY', 'NO_TRADING', $1, $2, $3)",
        )
        .bind(start)
        .bind(end)
        .bind(NOW_MS)
        .execute(pool)
        .await
        .expect("seed closure");
    }

    async fn setup_state(pool: &PgPool, signal_id: uuid::Uuid) -> String {
        sqlx::query_scalar("SELECT state->>'setupState' FROM tracker_states WHERE signal_id = $1")
            .bind(signal_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn repair_at_latest_close(pool: &PgPool) {
        repair_terminal_recommendation_states_at_latest_close(
            pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            None,
        )
        .await
        .unwrap();
    }

    fn tick(id: i64, time: i64, price: i64) -> SupervisorEvent {
        SupervisorEvent::Market(MarketEvent::AggregateTrade(
            market_domain::AggregateTrade::new(
                test_symbol(),
                id,
                id,
                id,
                time,
                time,
                Decimal::new(price, 0),
            )
            .unwrap(),
        ))
    }

    #[tokio::test]
    async fn repair_inside_a_closure_keeps_a_setup_the_price_moved_past() {
        let (pool, _guard) = test_pool().await;
        let signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        seed_signal(&pool, &signal).await;
        // The latest candle closes at 4135, past the 4130 target, while the market is closed.
        seed_latest_candle(&pool).await;
        seed_no_trading_session(&pool, NOW_MS, NOW_MS + 10 * M1_MS).await;

        repair_at_latest_close(&pool).await;

        assert_eq!(
            setup_state(&pool, signal.signal_id).await,
            "ENTRY_AVAILABLE"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn repair_inside_a_closure_still_expires_a_setup() {
        let (pool, _guard) = test_pool().await;
        let mut signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        signal.expires_at = Some(NOW_MS + 1);
        seed_signal(&pool, &signal).await;
        seed_latest_candle(&pool).await;
        seed_no_trading_session(&pool, NOW_MS, NOW_MS + 10 * M1_MS).await;

        repair_at_latest_close(&pool).await;

        assert_eq!(setup_state(&pool, signal.signal_id).await, "EXPIRED");
        pool.close().await;
    }

    #[tokio::test]
    async fn limit_setup_waits_out_a_closure_across_restore_and_restart() {
        let (pool, _guard) = test_pool().await;
        let signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        seed_signal(&pool, &signal).await;
        let reopening = NOW_MS + 10 * M1_MS;
        seed_no_trading_session(&pool, NOW_MS, reopening).await;

        let tracker = Arc::new(TrackerRepository::new(pool.clone()));
        let mut supervisor = ExecutionSupervisor::new(tracker.clone());
        supervisor.restore_active(&test_symbol()).await.unwrap();
        refresh_market_closures(&mut supervisor, &pool, &test_symbol())
            .await
            .unwrap();
        supervisor
            .process(tick(1, NOW_MS + 1, 4_105))
            .await
            .unwrap();
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        // A recovery restores the tracker in place and keeps the closures it was given.
        supervisor.restore_active(&test_symbol()).await.unwrap();
        supervisor
            .process(tick(2, NOW_MS + 2, 4_105))
            .await
            .unwrap();
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        // A process restart loads them again.
        let mut restarted = ExecutionSupervisor::new(tracker);
        restarted.restore_active(&test_symbol()).await.unwrap();
        refresh_market_closures(&mut restarted, &pool, &test_symbol())
            .await
            .unwrap();
        restarted.process(tick(3, NOW_MS + 3, 4_105)).await.unwrap();
        assert_eq!(trade_entry(&pool, signal.signal_id).await, None);

        restarted.process(tick(4, reopening, 4_105)).await.unwrap();
        assert_eq!(
            trade_entry(&pool, signal.signal_id).await,
            Some((reopening, Decimal::new(4_105, 0)))
        );
        pool.close().await;
    }

    fn historical_buy_signal() -> SignalDefinition {
        let mut signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        signal.plan = PlanKind::TrendBreakout;
        signal.target_timeframe = Timeframe::M5;
        signal.execution_instruction = ExecutionInstruction::NextOpen;
        signal.entry_zone = None;
        signal.parameter_version = "trend-5m-v1".to_owned();
        signal
    }

    /// The entry bar stays between the 4090 stop and the 4130 target.
    async fn seed_historical_entry(pool: &PgPool, signal: &SignalDefinition) {
        seed_pending_signal(pool, signal).await;
        seed_candle(
            pool,
            Timeframe::M5,
            signal.effective_at,
            Decimal::new(4105, 0),
            Decimal::new(4110, 0),
            Decimal::new(4100, 0),
            Decimal::new(4105, 0),
        )
        .await;
    }

    async fn seed_m1(pool: &PgPool, open_time: i64, high: i64, low: i64) {
        let close = Decimal::new((high + low) / 2, 0);
        seed_candle(
            pool,
            Timeframe::M1,
            open_time,
            close,
            Decimal::new(high, 0),
            Decimal::new(low, 0),
            close,
        )
        .await;
    }

    async fn deliver_pending_inputs(supervisor: &mut ExecutionSupervisor, pool: &PgPool) {
        process_pending_execution_inputs(
            supervisor,
            pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            &mut HashSet::new(),
            &mut HashSet::new(),
            None,
        )
        .await
        .unwrap();
    }

    async fn trade_outcome(
        pool: &PgPool,
        signal: &SignalDefinition,
    ) -> (String, Option<i64>, Option<Decimal>) {
        let row = sqlx::query(
            "SELECT trade_state, exit_time, exit_price FROM virtual_trades WHERE signal_id = $1",
        )
        .bind(signal.signal_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (
            row.get("trade_state"),
            row.get("exit_time"),
            row.get("exit_price"),
        )
    }

    #[tokio::test]
    async fn historical_trade_ignores_the_latest_price_until_replayed() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        seed_historical_entry(&pool, &signal).await;
        // Long after entry, price sits far below the stop.
        seed_m1(&pool, signal.effective_at + 60 * M1_MS, 4001, 3999).await;

        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        deliver_pending_inputs(&mut supervisor, &pool).await;

        let (state, exit_time, _) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "OPEN");
        assert_eq!(exit_time, None);
        pool.close().await;
    }

    async fn opened_historical_trade(pool: &PgPool) -> ExecutionSupervisor {
        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        deliver_pending_inputs(&mut supervisor, pool).await;
        supervisor
    }

    #[tokio::test]
    async fn replay_closes_a_historical_trade_at_the_candle_that_touched_its_target() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let target_candle = signal.effective_at + M5_MS + M1_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_m1(&pool, signal.effective_at + M5_MS, 4106, 4104).await;
        seed_m1(&pool, target_candle, 4131, 4104).await;
        seed_m1(&pool, signal.effective_at + 60 * M1_MS, 4001, 3999).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        replay_open_trades_through(
            &mut supervisor,
            &pool,
            &test_symbol(),
            signal.effective_at + 60 * M1_MS,
        )
        .await
        .unwrap();

        let (state, exit_time, exit_price) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "CLOSED");
        assert_eq!(exit_time, Some(target_candle + M1_MS - 1));
        assert_eq!(exit_price, Some(Decimal::new(4130, 0)));
        pool.close().await;
    }

    #[tokio::test]
    async fn replay_stops_at_the_bar_analysis_has_reached() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let target_candle = signal.effective_at + M5_MS + M1_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_m1(&pool, signal.effective_at + M5_MS, 4106, 4104).await;
        seed_m1(&pool, target_candle, 4131, 4104).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        replay_open_trades_through(&mut supervisor, &pool, &test_symbol(), target_candle - 1)
            .await
            .unwrap();

        assert_eq!(trade_outcome(&pool, &signal).await.0, "OPEN");
        pool.close().await;
    }

    #[tokio::test]
    async fn live_market_data_does_not_reach_a_replaying_trade() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let live_time = signal.effective_at + 60 * M1_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_m1(&pool, live_time, 4106, 4104).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        let live_candle = market_domain::Candle::new(
            test_symbol(),
            Timeframe::M1,
            live_time + M1_MS,
            live_time + 2 * M1_MS - 1,
            Decimal::new(4000, 0),
            Decimal::new(4001, 0),
            Decimal::new(3999, 0),
            Decimal::new(4000, 0),
        )
        .unwrap();
        let live_tick = market_domain::AggregateTrade::new(
            test_symbol(),
            10,
            10,
            10,
            live_time + 2 * M1_MS,
            live_time + 2 * M1_MS,
            Decimal::new(4000, 0),
        )
        .unwrap();
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::ClosedCandle(
                live_candle,
            )))
            .await
            .unwrap();
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                live_tick,
            )))
            .await
            .unwrap();

        assert_eq!(trade_outcome(&pool, &signal).await.0, "OPEN");
        pool.close().await;
    }

    #[tokio::test]
    async fn caught_up_trade_returns_to_live_market_data() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let last_candle = signal.effective_at + M5_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_m1(&pool, last_candle, 4106, 4104).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        replay_open_trades_through(
            &mut supervisor,
            &pool,
            &test_symbol(),
            last_candle + M1_MS - 1,
        )
        .await
        .unwrap();
        let tick_time = last_candle + M1_MS;
        let live_tick = market_domain::AggregateTrade::new(
            test_symbol(),
            10,
            10,
            10,
            tick_time,
            tick_time,
            Decimal::new(4000, 0),
        )
        .unwrap();
        supervisor
            .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(
                live_tick,
            )))
            .await
            .unwrap();

        let (state, exit_time, _) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "CLOSED");
        assert_eq!(exit_time, Some(tick_time));
        pool.close().await;
    }

    #[tokio::test]
    async fn recovery_leaves_a_replaying_trade_to_the_replay() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let stop_candle = signal.effective_at + M5_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_m1(&pool, stop_candle, 4001, 3999).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        supervisor.restore_active(&test_symbol()).await.unwrap();
        recovery::reconcile_active_from_m1(&mut supervisor, &pool, &test_symbol())
            .await
            .unwrap();
        assert_eq!(trade_outcome(&pool, &signal).await.0, "OPEN");

        replay_open_trades_through(
            &mut supervisor,
            &pool,
            &test_symbol(),
            stop_candle + M1_MS - 1,
        )
        .await
        .unwrap();
        let (state, exit_time, exit_price) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "CLOSED");
        assert_eq!(exit_time, Some(stop_candle + M1_MS - 1));
        assert_eq!(exit_price, Some(Decimal::new(4090, 0)));
        pool.close().await;
    }

    #[tokio::test]
    async fn replay_waits_for_backfill_instead_of_skipping_missing_candles() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let target_candle = signal.effective_at + M5_MS;
        let later_candle = signal.effective_at + 60 * M1_MS;
        seed_historical_entry(&pool, &signal).await;
        // Backfill has not reached the candles right after the entry bar yet.
        seed_m1(&pool, later_candle, 4001, 3999).await;
        sqlx::query(
            "INSERT INTO sync_states (symbol, interval, oldest_synced_time, full_history_complete) \
             VALUES ($1, '1m', $2, FALSE) \
             ON CONFLICT (symbol, interval) DO UPDATE \
             SET oldest_synced_time = $2, full_history_complete = FALSE",
        )
        .bind(test_symbol().as_str())
        .bind(later_candle)
        .execute(&pool)
        .await
        .unwrap();

        let mut supervisor = opened_historical_trade(&pool).await;
        let reached = replay_open_trades_through(
            &mut supervisor,
            &pool,
            &test_symbol(),
            later_candle + M1_MS,
        )
        .await
        .unwrap();
        assert!(!reached);
        assert_eq!(trade_outcome(&pool, &signal).await.0, "OPEN");

        seed_m1(&pool, target_candle, 4131, 4104).await;
        sqlx::query(
            "UPDATE sync_states SET oldest_synced_time = $1 WHERE symbol = $2 AND interval = '1m'",
        )
        .bind(target_candle)
        .bind(test_symbol().as_str())
        .execute(&pool)
        .await
        .unwrap();
        let reached = replay_open_trades_through(
            &mut supervisor,
            &pool,
            &test_symbol(),
            later_candle + M1_MS,
        )
        .await
        .unwrap();

        assert!(reached);
        let (state, exit_time, exit_price) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "CLOSED");
        assert_eq!(exit_time, Some(target_candle + M1_MS - 1));
        assert_eq!(exit_price, Some(Decimal::new(4130, 0)));
        pool.close().await;
    }

    #[tokio::test]
    async fn close_decision_from_an_earlier_bar_applies_before_later_candles() {
        let (pool, _guard) = test_pool().await;
        let signal = historical_buy_signal();
        let next_bar = signal.effective_at + M5_MS;
        seed_historical_entry(&pool, &signal).await;
        seed_candle(
            &pool,
            Timeframe::M5,
            next_bar,
            Decimal::new(4101, 0),
            Decimal::new(4131, 0),
            Decimal::new(4100, 0),
            Decimal::new(4110, 0),
        )
        .await;
        // This candle would hit the target if the close decision were ignored.
        seed_m1(&pool, next_bar, 4131, 4100).await;

        let mut supervisor = opened_historical_trade(&pool).await;
        let virtual_trade_id: uuid::Uuid =
            sqlx::query_scalar("SELECT virtual_trade_id FROM virtual_trades WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let directive = ManagementDirective {
            directive_id: uuid::Uuid::now_v7(),
            signal_id: signal.signal_id,
            virtual_trade_id: Some(virtual_trade_id),
            kind: DirectiveKind::SuggestClose,
            effective_at: next_bar,
            price: None,
            reason: "test close advice".to_owned(),
            source_closed_candle_time: next_bar - 1,
            algorithm_version: signal.algorithm_version.clone(),
            parameter_version: signal.parameter_version.clone(),
            reverse_signal: None,
        };
        sqlx::query(
            "INSERT INTO trade_management_directives \
             (directive_id, signal_id, virtual_trade_id, kind, payload, effective_at, \
              source_closed_candle_time, algorithm_version, parameter_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(directive.directive_id)
        .bind(directive.signal_id)
        .bind(directive.virtual_trade_id)
        .bind(directive.kind.as_str())
        .bind(serde_json::to_value(&directive).unwrap())
        .bind(directive.effective_at)
        .bind(directive.source_closed_candle_time)
        .bind(&directive.algorithm_version)
        .bind(&directive.parameter_version)
        .execute(&pool)
        .await
        .unwrap();

        deliver_pending_inputs(&mut supervisor, &pool).await;
        replay_open_trades_through(&mut supervisor, &pool, &test_symbol(), next_bar + M1_MS - 1)
            .await
            .unwrap();

        let (state, exit_time, exit_price) = trade_outcome(&pool, &signal).await;
        assert_eq!(state, "CLOSED");
        assert_eq!(exit_time, Some(next_bar));
        assert_eq!(exit_price, Some(Decimal::new(4101, 0)));
        pool.close().await;
    }

    #[tokio::test]
    async fn restored_next_open_signal_replays_stored_canonical_open() {
        let (pool, _guard) = test_pool().await;
        let mut signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        signal.plan = PlanKind::TrendBreakout;
        signal.target_timeframe = Timeframe::M5;
        signal.execution_instruction = ExecutionInstruction::NextOpen;
        signal.entry_zone = None;
        signal.projected_entry = Some(Decimal::new(417_849, 2));
        signal.stop_loss = Decimal::new(417_702, 2);
        signal.primary_target = Decimal::new(418_069, 2);
        signal.parameter_version = "trend-5m-v1".to_owned();
        seed_signal(&pool, &signal).await;
        seed_candle(
            &pool,
            Timeframe::M5,
            signal.effective_at,
            Decimal::new(417_849, 2),
            Decimal::new(417_899, 2),
            Decimal::new(417_849, 2),
            Decimal::new(417_882, 2),
        )
        .await;

        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        process_active_next_open_canonical_opens(&mut supervisor, &pool)
            .await
            .unwrap();

        let trade_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM virtual_trades WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(trade_count, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn close_advice_waits_for_and_executes_at_stored_canonical_open() {
        let (pool, _guard) = test_pool().await;
        let signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        seed_signal(&pool, &signal).await;

        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        let entry = market_domain::AggregateTrade::new(
            test_symbol(),
            10,
            10,
            10,
            NOW_MS,
            NOW_MS,
            Decimal::new(4105, 0),
        )
        .unwrap();
        assert_eq!(
            supervisor
                .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(entry)))
                .await
                .unwrap(),
            SupervisorStep::Committed
        );

        let virtual_trade_id: uuid::Uuid =
            sqlx::query_scalar("SELECT virtual_trade_id FROM virtual_trades WHERE signal_id = $1")
                .bind(signal.signal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let effective_at = NOW_MS + Timeframe::M1.duration_ms();
        let directive = ManagementDirective {
            directive_id: uuid::Uuid::now_v7(),
            signal_id: signal.signal_id,
            virtual_trade_id: Some(virtual_trade_id),
            kind: DirectiveKind::SuggestClose,
            effective_at,
            price: None,
            reason: "test close advice".to_owned(),
            source_closed_candle_time: effective_at - 1,
            algorithm_version: signal.algorithm_version.clone(),
            parameter_version: signal.parameter_version.clone(),
            reverse_signal: None,
        };
        sqlx::query(
            "INSERT INTO trade_management_directives \
             (directive_id, signal_id, virtual_trade_id, kind, payload, effective_at, \
              source_closed_candle_time, algorithm_version, parameter_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(directive.directive_id)
        .bind(directive.signal_id)
        .bind(directive.virtual_trade_id)
        .bind(directive.kind.as_str())
        .bind(serde_json::to_value(&directive).unwrap())
        .bind(directive.effective_at)
        .bind(directive.source_closed_candle_time)
        .bind(&directive.algorithm_version)
        .bind(&directive.parameter_version)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            pending_execution_directives(&pool, effective_at, 100)
                .await
                .unwrap()
                .is_empty(),
            "price-less close advice must remain pending until its canonical candle exists"
        );

        let canonical_open = Decimal::new(4112, 0);
        seed_candle(
            &pool,
            Timeframe::M1,
            effective_at,
            canonical_open,
            Decimal::new(4116, 0),
            Decimal::new(4110, 0),
            Decimal::new(4114, 0),
        )
        .await;
        process_pending_execution_inputs(
            &mut supervisor,
            &pool,
            Arc::new(CandleRepository::new(pool.clone())),
            &test_symbol(),
            &mut HashSet::new(),
            &mut HashSet::new(),
            None,
        )
        .await
        .unwrap();

        let row = sqlx::query(
            "SELECT trade_state, exit_price, result->>'exitReason' AS exit_reason \
             FROM virtual_trades WHERE virtual_trade_id = $1",
        )
        .bind(virtual_trade_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("trade_state"), "CLOSED");
        assert_eq!(
            row.get::<Option<Decimal>, _>("exit_price"),
            Some(canonical_open)
        );
        assert_eq!(
            row.get::<Option<String>, _>("exit_reason").as_deref(),
            Some("test close advice")
        );
        let consumed: bool = sqlx::query_scalar(
            "SELECT consumed_at IS NOT NULL FROM trade_management_directives WHERE directive_id = $1",
        )
        .bind(directive.directive_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(consumed);
        pool.close().await;
    }

    #[tokio::test]
    async fn runtime_gap_status_reports_missing_expected_candle_inside_regular_session() {
        let (pool, _guard) = test_pool().await;
        seed_regular_session(&pool, 0, 300_000).await;
        seed_gap_candles(&pool).await;
        let gaps = RuntimeGapStatusSource::new(
            Arc::new(MarketRepository::new(pool.clone())),
            Arc::new(CandleRepository::new(pool.clone())),
            Arc::new(SessionRepository::new(pool.clone())),
        );

        assert!(
            gaps.has_unresolved_gap(&test_symbol(), Timeframe::M1, 239_999)
                .await
                .unwrap()
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn exact_replay_runs_past_five_pages_and_checkpoints_final_cursor() {
        let (pool, _guard) = test_pool().await;
        sqlx::query(
            "INSERT INTO market_event_cursors \
             (symbol, last_processed_agg_trade_id, last_trade_time) \
             VALUES ('XAUUSDT', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let source = Arc::new(PagedTradeSource {
            max_id: 5_500,
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();

        replay_unprocessed_aggregate_trades(&mut supervisor, source.clone(), &pool, &test_symbol())
            .await
            .unwrap();

        let cursor: i64 = sqlx::query_scalar(
            "SELECT last_processed_agg_trade_id FROM market_event_cursors WHERE symbol = 'XAUUSDT'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor, 5_500);
        assert!(source.calls.lock().unwrap().len() >= 6);
        pool.close().await;
    }

    #[tokio::test]
    async fn recovery_uses_m1_ohlc_when_exact_replay_has_no_trade_history() {
        let (pool, _guard) = test_pool().await;
        let signal = test_signal(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        seed_signal(&pool, &signal).await;
        let mut supervisor =
            ExecutionSupervisor::new(Arc::new(TrackerRepository::new(pool.clone())));
        supervisor.restore_active(&test_symbol()).await.unwrap();
        let entry = market_domain::AggregateTrade::new(
            test_symbol(),
            10,
            10,
            10,
            NOW_MS,
            NOW_MS,
            Decimal::new(4105, 0),
        )
        .unwrap();
        assert_eq!(
            supervisor
                .process(SupervisorEvent::Market(MarketEvent::AggregateTrade(entry)))
                .await
                .unwrap(),
            SupervisorStep::Committed
        );
        seed_candle(
            &pool,
            Timeframe::M1,
            NOW_MS + Timeframe::M1.duration_ms(),
            Decimal::new(4105, 0),
            Decimal::new(4120, 0),
            Decimal::new(4089, 0),
            Decimal::new(4100, 0),
        )
        .await;
        let source = Arc::new(PagedTradeSource {
            max_id: 10,
            calls: Arc::new(Mutex::new(Vec::new())),
        });

        recover_execution_tracking(&mut supervisor, source, &pool, &test_symbol())
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT trade_state, reconciliation_source, ambiguity, result->>'exitReason' AS exit_reason \
             FROM virtual_trades WHERE signal_id = $1",
        )
        .bind(signal.signal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("trade_state"), "CLOSED");
        assert_eq!(
            row.get::<Option<String>, _>("reconciliation_source")
                .as_deref(),
            Some("OHLC_RECONCILED")
        );
        assert_eq!(row.get::<String, _>("ambiguity"), "NONE");
        assert_eq!(row.get::<String, _>("exit_reason"), "initial stop touched");
        pool.close().await;
    }
}
