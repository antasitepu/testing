//! Read-only historical baseline, separate from parameter tuning and promotion.
use super::*;
use analysis_domain::{
    CanonicalMarketSnapshot, MaterializedProfile, ReadinessReason, TimeframeFacts,
};
use analysis_plans::{PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState};
use backtest_engine::{
    BacktestReplay, BacktestRunInput, CandlePriceReference, FeeConfig, ReplayFrame, SlippageConfig,
    SpreadConfig,
};
use market_domain::Candle;
use std::collections::BTreeMap;

struct RuntimeEntryPolicy {
    evaluator: Arc<dyn PlanEvaluator>,
}

impl PlanEvaluator for RuntimeEntryPolicy {
    fn kind(&self) -> PlanKind {
        self.evaluator.kind()
    }

    fn algorithm_version(&self) -> &'static str {
        self.evaluator.algorithm_version()
    }

    fn evaluate(
        &self,
        profile: &MaterializedProfile,
        snapshot: &CanonicalMarketSnapshot,
        state: &PlanRuntimeState,
        feedback: &[analysis_domain::PlanFeedback],
    ) -> Result<PlanEvaluation, PlanError> {
        // Historical candles model live arrivals. Preserve the live worker's M1
        // entry restriction; this is not the suppressed non-live catch-up path.
        if analysis_service::suppresses_new_rsi_m1_entry(&profile.key, state) {
            return Ok(PlanEvaluation {
                next_state: state.clone(),
                signal: None,
                events: Vec::new(),
                directives: Vec::new(),
                rejected_candidates: Vec::new(),
            });
        }
        self.evaluator.evaluate(profile, snapshot, state, feedback)
    }
}

struct History {
    candles: Vec<Candle>,
    raw_opens: Vec<i64>,
    missing_opens: Vec<i64>,
}

fn has_recent_gap(history: &History, timeframe: Timeframe, close: i64) -> bool {
    let end = close + 1;
    let start = history.raw_opens[0].max(end - timeframe.duration_ms() * 200);
    let first = history.missing_opens.partition_point(|open| *open < start);
    history
        .missing_opens
        .get(first)
        .is_some_and(|open| *open + timeframe.duration_ms() <= end)
}

fn fact_window(history: &History, end: usize) -> &[Candle] {
    let close = history.candles[end].close_time;
    let raw_end = history.raw_opens.partition_point(|open| *open <= close);
    // Match the runtime's latest 1000 raw candles before session filtering.
    let first_open = history.raw_opens[raw_end.saturating_sub(1000)];
    let start = history.candles[..=end].partition_point(|candle| candle.open_time < first_open);
    &history.candles[start..=end]
}

fn facts_for_window(window: &[Candle]) -> Vec<ConfirmedFact<Value>> {
    if window.len() < 50 {
        Vec::new()
    } else {
        build_facts(window).expect("runtime fact construction")
    }
}

#[tokio::test]
#[ignore = "manual 42-profile read-only baseline; requires local candle history"]
async fn print_runtime_baseline_from_database() {
    let _ = dotenvy::dotenv();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let max_bars = std::env::var("GOLD_BASELINE_MAX_BARS")
        .map(|value| value.parse::<usize>().expect("positive max bars"))
        .unwrap_or(10_000);
    assert!(max_bars >= 5);
    let evaluated_timeframes: Vec<Timeframe> = match std::env::var("GOLD_BASELINE_TIMEFRAMES") {
        Ok(list) => list
            .split(',')
            .map(|name| {
                Timeframe::ALL
                    .into_iter()
                    .find(|timeframe| timeframe.as_str() == name)
                    .unwrap_or_else(|| panic!("unknown timeframe {name}"))
            })
            .collect(),
        Err(_) => Timeframe::ALL.to_vec(),
    };
    let spread_price = std::env::var("GOLD_BASELINE_SPREAD_PRICE")
        .map(|value| value.parse::<Decimal>().expect("decimal spread price"))
        .unwrap_or(Decimal::ZERO);
    let reference = match std::env::var("GOLD_BASELINE_CANDLE_REFERENCE").as_deref() {
        Ok("MID") => CandlePriceReference::Mid,
        Ok("BID") => CandlePriceReference::Bid,
        Ok("ASK") => CandlePriceReference::Ask,
        Err(_) if spread_price.is_zero() => CandlePriceReference::Mid,
        _ => panic!("set GOLD_BASELINE_CANDLE_REFERENCE to MID, BID or ASK explicitly"),
    };
    let spread = SpreadConfig {
        price: spread_price,
        candle_reference: reference,
    }
    .validate()
    .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("connect read-only baseline database");
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let cutoff: i64 = sqlx::query_scalar(
        "SELECT MIN(latest_close) FROM (SELECT MAX(close_time) AS latest_close \
         FROM candles WHERE symbol = $1 AND interval = ANY($2) \
         AND close_time <= (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT \
         GROUP BY interval) closed_history",
    )
    .bind(symbol.as_str())
    .bind(Timeframe::ALL.map(|timeframe| timeframe.as_str()).to_vec())
    .fetch_one(&pool)
    .await
    .expect("common closed-candle cutoff");
    let cutoff = std::env::var("GOLD_BASELINE_CUTOFF")
        .map(|value| {
            let pinned = value.parse::<i64>().expect("integer cutoff");
            assert!(
                pinned <= cutoff,
                "pinned cutoff exceeds available closed history"
            );
            pinned
        })
        .unwrap_or(cutoff);
    let overrides = crate::session_history::parse_session_overrides(
        include_str!("../../../../config/commodity-session-overrides.json"),
        "COMMODITY",
    )
    .unwrap();
    let loader = crate::tuning_dataset::TuningDatasetLoader::new(
        pool.clone(),
        "commodity-session-policy-v2",
        overrides.version,
    );
    let mut histories = BTreeMap::new();
    let session_market: String =
        sqlx::query_scalar("SELECT session_market FROM markets WHERE symbol = $1")
            .bind(symbol.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    for timeframe in Timeframe::ALL {
        // Candle close = open + duration - 1; the loader's bound is exclusive.
        let before = cutoff - timeframe.duration_ms() + 2;
        let dataset = loader
            .load(&symbol, timeframe, Some(before), max_bars + 1000)
            .await
            .expect("load session-clean baseline history");
        assert!(
            dataset.candles.len() >= 5,
            "insufficient history for {}",
            timeframe.as_str()
        );
        assert!(
            dataset
                .candles
                .iter()
                .all(|candle| candle.close_time <= cutoff)
        );
        println!(
            "{}",
            crate::tuning_dataset::dataset_audit_line(&dataset.audit)
        );
        let raw_opens: Vec<i64> = sqlx::query_scalar(
            "SELECT open_time FROM candles WHERE symbol = $1 AND interval = $2 \
             AND open_time < $3 ORDER BY open_time",
        )
        .bind(symbol.as_str())
        .bind(timeframe.as_str())
        .bind(before)
        .fetch_all(&pool)
        .await
        .expect("raw history window timestamps");
        let sessions = SessionRepository::new(pool.clone())
            .between(&session_market, raw_opens[0], cutoff + 1)
            .await
            .unwrap();
        let present: HashSet<_> = raw_opens.iter().copied().collect();
        let missing_opens =
            market_service::expected_open_times(timeframe, raw_opens[0], cutoff + 1, &sessions)
                .into_iter()
                .filter(|open| !present.contains(open))
                .collect();
        histories.insert(
            timeframe,
            History {
                candles: dataset.candles,
                raw_opens,
                missing_opens,
            },
        );
    }
    println!(
        "BASELINE_PROTOCOL,{}",
        json!({
            "cutoff": cutoff, "maxBars": max_bars, "rawWindow": 1000,
            "historicalArrivalMode": "LIVE_POLICY", "rsiM1EntrySuppressed": true,
            "fees": "0", "extraSlippage": "0", "spread": spread, "executionModel": "OHLC_CONSERVATIVE",
            "databaseWrites": false,
        })
    );

    // Every history stays loaded, because a timeframe reads its higher timeframe's candles.
    for timeframe in evaluated_timeframes {
        let history = &histories[&timeframe];
        let start = history.candles.len().saturating_sub(max_bars);
        let evaluation_candles = &history.candles[start..];
        let registry = runtime_plan_registry();
        let cases: Vec<_> = PlanKind::ALL
            .into_iter()
            .map(|plan| {
                let mut profile =
                    default_runtime_profile(uuid::Uuid::now_v7(), symbol.clone(), plan, timeframe)
                        .unwrap();
                let runtime_algorithm = profile.versions.algorithm_version.clone();
                let evaluator = RuntimeEntryPolicy {
                    evaluator: registry.get(plan).expect("registered plan").clone(),
                };
                // Replay checks the evaluator identity; retain the runtime identity in output.
                profile.versions.algorithm_version = evaluator.algorithm_version().to_owned();
                (profile, evaluator, runtime_algorithm)
            })
            .collect();
        let mut sessions: Vec<_> = cases
            .iter()
            .map(|(profile, evaluator, _)| {
                BacktestReplay::new(
                    BacktestRunInput {
                        symbol: symbol.clone(),
                        plan: profile.key.plan,
                        target_timeframe: timeframe,
                        start_time: evaluation_candles.first().unwrap().open_time,
                        end_time: evaluation_candles.last().unwrap().close_time,
                        release_id: profile.versions.release_id,
                        algorithm_version: profile.versions.algorithm_version.clone(),
                        parameter_version: profile.versions.parameter_version.clone(),
                        fees: FeeConfig::default(),
                        slippage: SlippageConfig::default(),
                        account: None,
                        code_commit_hash: "local-runtime-baseline".to_owned(),
                    },
                    profile.clone(),
                )
                .with_spread(spread)
                .unwrap()
                .start_session(evaluator)
                .unwrap()
            })
            .collect();
        let mut higher_cache: Option<(usize, TimeframeFacts, usize)> = None;
        let mut gap_frames = 0_usize;
        for index in start..history.candles.len() {
            let candle = &history.candles[index];
            let window = fact_window(history, index);
            let target_count = window.len();
            let unresolved_gap = has_recent_gap(history, timeframe, candle.close_time);
            gap_frames += usize::from(unresolved_gap);
            let target = TimeframeFacts {
                timeframe,
                evaluated_closed_candle_time: candle.close_time,
                facts: facts_for_window(window),
            };
            let higher = next_higher_timeframe(timeframe).and_then(|higher_timeframe| {
                let higher_history = &histories[&higher_timeframe];
                let end = higher_history
                    .candles
                    .partition_point(|candle| {
                        candle.close_time <= target.evaluated_closed_candle_time
                    })
                    .checked_sub(1)?;
                if higher_cache
                    .as_ref()
                    .is_none_or(|(cached, _, _)| *cached != end)
                {
                    let higher_window = fact_window(higher_history, end);
                    higher_cache = Some((
                        end,
                        TimeframeFacts {
                            timeframe: higher_timeframe,
                            evaluated_closed_candle_time: higher_history.candles[end].close_time,
                            facts: facts_for_window(higher_window),
                        },
                        higher_window.len(),
                    ));
                }
                higher_cache
                    .as_ref()
                    .map(|(_, facts, count)| (facts.clone(), *count))
            });
            let mut frame = ReplayFrame {
                candle: candle.clone(),
                snapshot: CanonicalMarketSnapshot {
                    symbol: symbol.clone(),
                    target_timeframe: timeframe,
                    price_tick: Decimal::new(1, 2),
                    evaluated_closed_candle_time: candle.close_time,
                    readiness: Readiness::Ready,
                    target,
                    higher_timeframes: higher
                        .as_ref()
                        .map(|(facts, _)| facts.clone())
                        .into_iter()
                        .collect(),
                    primitive_versions: Vec::new(),
                },
                directives: Vec::new(),
                warmup: false,
            };
            for (session, (profile, _, _)) in sessions.iter_mut().zip(&cases) {
                frame.snapshot.readiness = if unresolved_gap {
                    Readiness::NotReady {
                        reason: ReadinessReason::UnresolvedGap,
                    }
                } else if target_count < profile.minimum_history_bars {
                    Readiness::NotReady {
                        reason: ReadinessReason::InsufficientHistory,
                    }
                } else if !profile.required_timeframes.is_empty()
                    && higher
                        .as_ref()
                        .is_none_or(|(_, count)| *count < profile.minimum_history_bars)
                {
                    Readiness::NotReady {
                        reason: ReadinessReason::MissingHigherTimeframe,
                    }
                } else {
                    Readiness::Ready
                };
                session.process_frame(&frame).unwrap();
            }
            if (index - start + 1) % 1000 == 0 {
                println!(
                    "BASELINE_PROGRESS,{},{}",
                    timeframe.as_str(),
                    index - start + 1
                );
            }
        }
        for (session, (profile, _, runtime_algorithm)) in sessions.into_iter().zip(&cases) {
            let result = session.finish(evaluation_candles.len() as u64);
            println!(
                "BASELINE_CASE,{}",
                json!({
                    "timeframe": timeframe.as_str(), "plan": profile.key.plan.as_str(),
                    "runtimeAlgorithm": runtime_algorithm, "evaluatorAlgorithm": profile.versions.algorithm_version,
                    "parameterVersion": profile.versions.parameter_version, "parameters": profile.parameters,
                    "requiredTimeframes": profile.required_timeframes, "historyCount": history.candles.len(),
                "evaluationCount": evaluation_candles.len(), "prehistoryCount": start,
                "gapFrames": gap_frames,
                    "start": evaluation_candles.first().unwrap().open_time, "end": evaluation_candles.last().unwrap().close_time,
                    "trainEnd": evaluation_candles[evaluation_candles.len() * 60 / 100 - 1].close_time,
                    "validationEnd": evaluation_candles[evaluation_candles.len() * 80 / 100 - 1].close_time,
                "spread": result.spread, "metrics": result.metrics, "trades": result.trades,
                "executions": result.executions, "positionManagement": profile.position_management,
                    "entries": result.executions.iter().filter(|e| matches!(e.kind, backtest_engine::ExecutionKind::Entry | backtest_engine::ExecutionKind::ReverseEntry)).count(),
                    "ambiguousExecutions": result.executions.iter().filter(|e| e.ambiguous_intrabar).count(),
                })
            );
        }
    }
    pool.close().await;
}

#[test]
fn fact_window_counts_raw_candles_before_session_filtering() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let raw_opens: Vec<_> = (0..1200).map(|i| i * 60_000).collect();
    let candles = raw_opens
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, open)| {
            Candle::new(
                symbol.clone(),
                Timeframe::M1,
                *open,
                *open + 59_999,
                Decimal::ONE,
                Decimal::ONE,
                Decimal::ONE,
                Decimal::ONE,
            )
            .unwrap()
        })
        .collect();
    let history = History {
        candles,
        raw_opens,
        missing_opens: Vec::new(),
    };
    let window = fact_window(&history, history.candles.len() - 1);
    assert_eq!(window.len(), 500);
    assert_eq!(window.first().unwrap().open_time, 200 * 60_000);
}

#[test]
fn gap_policy_uses_the_same_200_bar_lookback_as_runtime() {
    let history = History {
        candles: Vec::new(),
        raw_opens: vec![0],
        missing_opens: vec![60_000],
    };
    assert!(has_recent_gap(&history, Timeframe::M1, 201 * 60_000 - 1));
    assert!(!has_recent_gap(&history, Timeframe::M1, 202 * 60_000 - 1));
    assert!(!has_recent_gap(&history, Timeframe::M1, 60_000 - 1));
}
