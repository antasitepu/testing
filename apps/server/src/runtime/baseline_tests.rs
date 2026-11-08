//! Read-only historical baseline, separate from parameter tuning and promotion.
use super::*;
use analysis_domain::{
    CanonicalMarketSnapshot, Direction, MaterializedProfile, ReadinessReason, TimeframeFacts,
};
use analysis_plans::{PlanError, PlanEvaluation, PlanEvaluator, PlanRuntimeState};
use backtest_engine::edge::{
    EDGE_V1, RealEntry, ResearchWindow, ResearchWindowDates, ResearchWindows, eligible_bars,
    evaluate_case, precompute_outcomes, score_entries,
};
use backtest_engine::hypotheses::{Hypothesis, day_cycle_entries, tsmom_entries};
use backtest_engine::volatility;
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

/// The runtime reads the latest 1000 raw candles before session filtering.
const RUNTIME_RAW_WINDOW: usize = 1000;

fn fact_window(history: &History, end: usize, raw_window: usize) -> &[Candle] {
    let close = history.candles[end].close_time;
    let raw_end = history.raw_opens.partition_point(|open| *open <= close);
    let first_open = history.raw_opens[raw_end.saturating_sub(raw_window)];
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

/// The plans' gap rule plus 200 bars of history; bars before the evaluated range
/// (`start`) are never ready.
fn research_readiness(history: &History, timeframe: Timeframe, start: usize) -> Vec<bool> {
    history
        .candles
        .iter()
        .enumerate()
        .map(|(index, candle)| {
            index >= start.max(200) && !has_recent_gap(history, timeframe, candle.close_time)
        })
        .collect()
}

/// Scores the range forecast benchmark (`vol-v1`) on one timeframe.
fn print_volatility(
    symbol: &str,
    timeframe: Timeframe,
    history: &History,
    max_bars: usize,
    windows: &ResearchWindows,
) {
    let candles = &history.candles;
    let start = candles.len().saturating_sub(max_bars);
    let ready_at_close = research_readiness(history, timeframe, start);
    let atr = analysis_core::calculate_atr(candles, volatility::ATR_PERIOD)
        .expect("vol ATR")
        .values;
    let report = volatility::evaluate(candles, &atr, &ready_at_close, windows, timeframe)
        .expect("vol-v1 evaluation");
    let identity = json!({ "symbol": symbol, "timeframe": timeframe.as_str() });
    let mut profile = identity.clone();
    profile["volVersion"] = json!(volatility::VOL_VERSION);
    profile["profile"] = json!(report.profile);
    println!("VOL_PROFILE,{profile}");
    for line in report.lines {
        let mut value = serde_json::to_value(&line).expect("vol line");
        for (key, field) in identity.as_object().unwrap() {
            value[key] = field.clone();
        }
        println!("VOL_CASE,{value}");
    }
    // vol-v3: scheduled-event terms from config/macro-events.json (public domain).
    if std::env::var("GOLD_BASELINE_VOL_EVENTS").as_deref() == Ok("1") {
        let (calendar_version, events) = load_macro_events();
        let mut calendar = identity.clone();
        calendar["calendarVersion"] = json!(calendar_version);
        calendar["events"] = json!(events.len());
        println!("VOL_EVENTS_CALENDAR,{calendar}");
        for line in
            volatility::evaluate_events(candles, &atr, &ready_at_close, windows, timeframe, &events)
                .expect("vol-v3 evaluation")
        {
            let mut value = serde_json::to_value(&line).expect("vol-v3 line");
            for (key, field) in identity.as_object().unwrap() {
                value[key] = field.clone();
            }
            println!("VOL_EVENTS,{value}");
        }
    }
    // vol-v3 on XAUUSDT: the fit on every origin before the vault, frozen for the
    // vault check. No window is scored, so no XAUUSDT event statistic is printed.
    if std::env::var("GOLD_BASELINE_VOL_EVENTS_FIT").as_deref() == Ok("1") {
        let (calendar_version, events) = load_macro_events();
        let (seasonal, coefficients) = volatility::event_fit_before_vault(
            candles,
            &atr,
            &ready_at_close,
            windows,
            timeframe,
            &events,
        )
        .expect("vol-v3 fit")
        .expect("vol-v3 fit is solvable");
        let mut value = identity.clone();
        value["volVersion"] = json!("vol-v3");
        value["calendarVersion"] = json!(calendar_version);
        value["coefficients"] = json!(coefficients);
        value["seasonalCoefficients"] = json!(seasonal.coefficients);
        value["profile"] = json!(seasonal.profile);
        value["origins"] = json!(seasonal.origins);
        println!("VOL_EVENTS_FIT,{value}");
    }
    // vol-v2: band quantiles and the refit candidate; no vault candle is a target.
    if std::env::var("GOLD_BASELINE_VOL_BAND").as_deref() == Ok("1") {
        let band = volatility::band_report(candles, &atr, &ready_at_close, windows, timeframe)
            .expect("vol-v2 band")
            .expect("vol-v2 band fits");
        for (fit, quantiles) in [
            (&band.live_fit, &band.live_band),
            (&band.refit, &band.refit_band),
        ] {
            let mut value = identity.clone();
            value["volVersion"] = json!("vol-v2");
            value["model"] = json!("HAR_RANGE_SEASONAL");
            value["band"] = serde_json::to_value(quantiles).expect("band");
            value["coefficients"] = json!(fit.coefficients);
            println!("VOL_BAND,{value}");
        }
        let mut refit = identity.clone();
        refit["volVersion"] = json!("vol-v2");
        refit["model"] = json!("HAR_RANGE_SEASONAL");
        refit["fit"] = serde_json::to_value(&band.refit).expect("refit");
        println!("VOL_REFIT,{refit}");
    }
}

/// True on GitHub Actions or any CI runner: the vault never enters CI.
fn running_in_ci(env: impl Fn(&str) -> Option<String>) -> bool {
    ["CI", "GITHUB_ACTIONS"]
        .iter()
        .any(|key| env(key).is_some_and(|value| !value.is_empty() && value != "false"))
}

/// A frozen range model from the private `research/models`, read at run time so
/// public builds need no private file.
fn load_range_model(variable: &str, file: &str) -> volatility::RangeModel {
    let path = std::env::var(variable)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../research/models")
                .join(file)
        });
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("range model JSON")
}

/// The vol-v2 vault check (Parts 1-3): residuals of the live model and of the
/// refit candidate, exactly as frozen, on every eligible vault origin.
fn print_vault_check(
    symbol: &str,
    history: &History,
    cutoff: i64,
    max_bars: usize,
    vault_start: i64,
) {
    let timeframe = Timeframe::M15;
    let candles = &history.candles;
    // Counted before anything is scored; a count-only run stops here.
    let trading_days = volatility::vault_trading_days(candles, vault_start, cutoff);
    if std::env::var("GOLD_VAULT_COUNT_ONLY").as_deref() == Ok("1") {
        println!(
            "VOL_VAULT_DAYS,{}",
            json!({ "symbol": symbol, "vaultStart": vault_start, "cutoff": cutoff, "tradingDays": trading_days })
        );
        return;
    }
    let start = candles.len().saturating_sub(max_bars);
    let ready_at_close = research_readiness(history, timeframe, start);
    let atr = analysis_core::calculate_atr(candles, volatility::ATR_PERIOD)
        .expect("vault ATR")
        .values;
    let origins = volatility::collect_origins(
        candles,
        &atr,
        &ready_at_close,
        &volatility::vault_windows(vault_start),
        timeframe,
    )
    .expect("vault origins")
    .origins;
    for (name, variable, file) in [
        (
            "live",
            "GOLD_VAULT_LIVE_MODEL",
            "range-forecast-xauusdt-15m-vol-v1.json",
        ),
        (
            "refit",
            "GOLD_VAULT_REFIT_MODEL",
            "range-forecast-xauusdt-15m-vol-v1-refit-candidate.json",
        ),
    ] {
        let model = load_range_model(variable, file);
        assert_eq!(model.validated_timeframe(), Ok(timeframe), "{file}");
        let scores = volatility::vault_scores(candles, &origins, &model, timeframe);
        let mut value = serde_json::to_value(&scores).expect("vault scores");
        value["symbol"] = json!(symbol);
        value["timeframe"] = json!(timeframe.as_str());
        value["model"] = json!(name);
        value["modelFile"] = json!(file);
        value["vaultStart"] = json!(vault_start);
        value["cutoff"] = json!(cutoff);
        value["tradingDays"] = json!(trading_days);
        println!("VOL_VAULT,{value}");
    }
    // vol-v3 on the same vault: the frozen candidate against its seasonal baseline.
    let file = "range-forecast-xauusdt-15m-vol-v3-candidate.json";
    let path = std::env::var("GOLD_VAULT_EVENTS_MODEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../research/models")
                .join(file)
        });
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let model: volatility::EventModel = serde_json::from_str(&text).expect("vol-v3 model JSON");
    let (calendar_version, events) = load_macro_events();
    assert_eq!(
        model.calendar_version, calendar_version,
        "calendar changed since the fit"
    );
    let scores = volatility::vault_event_scores(candles, &origins, &model, &events, timeframe);
    let mut value = serde_json::to_value(&scores).expect("vault event scores");
    value["symbol"] = json!(symbol);
    value["timeframe"] = json!(timeframe.as_str());
    value["model"] = json!("events");
    value["modelFile"] = json!(file);
    value["vaultStart"] = json!(vault_start);
    value["cutoff"] = json!(cutoff);
    value["tradingDays"] = json!(trading_days);
    println!("VOL_VAULT_EVENTS,{value}");
}

/// `config/macro-events.json`, read at run time: its version and sorted events.
fn load_macro_events() -> (String, Vec<volatility::MacroEvent>) {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/macro-events.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let file: volatility::MacroEventsFile = serde_json::from_str(&text).expect("macro events JSON");
    let events = file.events().expect("valid macro events");
    (file.version, events)
}

/// Scores one hypothesis with edge-v1, with the research readiness above.
fn print_hypothesis_edge(
    hypothesis: Hypothesis,
    histories: &BTreeMap<Timeframe, History>,
    max_bars: usize,
    windows: &ResearchWindows,
) {
    let timeframe = hypothesis.timeframe();
    let history = &histories[&timeframe];
    let candles = &history.candles;
    let start = candles.len().saturating_sub(max_bars);
    let ready_at_close = research_readiness(history, timeframe, start);
    let atr = analysis_core::calculate_atr(candles, EDGE_V1.atr_period)
        .expect("edge ATR")
        .values;
    let outcomes = precompute_outcomes(candles, &atr, &EDGE_V1).expect("edge outcomes");
    let eligible =
        eligible_bars(candles, &outcomes, &ready_at_close, windows).expect("edge eligibility");
    let entries = match hypothesis {
        Hypothesis::GoldDayCycle => day_cycle_entries(&candles[start..]),
        Hypothesis::Tsmom12m => tsmom_entries(
            &candles[start..],
            &histories[&Timeframe::D1].candles,
            EDGE_V1.time_stop_bars,
        ),
    };
    let case_key = format!(
        "{}/{}/{}/{}",
        timeframe.as_str(),
        hypothesis.name(),
        hypothesis.algorithm_version(),
        hypothesis.parameter_version()
    );
    let identity = json!({
        "timeframe": timeframe.as_str(),
        "plan": hypothesis.name(),
        "algorithmVersion": hypothesis.algorithm_version(),
        "parameterVersion": hypothesis.parameter_version(),
        "runtimeAlgorithm": "research-hypothesis",
    });
    for line in evaluate_case(
        candles, &outcomes, &eligible, &entries, windows, &case_key, &EDGE_V1,
    ) {
        let mut value = serde_json::to_value(&line).expect("edge line");
        for (key, field) in identity.as_object().unwrap() {
            value[key] = field.clone();
        }
        println!("EDGE_CASE,{value}");
    }
    // Diagnostic only, never deciding; the sealed test window is not described.
    for (window, scored) in score_entries(candles, &eligible, &entries, windows) {
        if window == ResearchWindow::Test {
            continue;
        }
        let side = |direction: Direction| {
            let rs: Vec<f64> = scored
                .scored
                .iter()
                .filter(|(_, side)| *side == direction)
                .filter_map(|(index, side)| outcomes.get(*index, *side).map(|outcome| outcome.r))
                .collect();
            let mean = (!rs.is_empty()).then(|| rs.iter().sum::<f64>() / rs.len() as f64);
            json!({ "entries": rs.len(), "meanR": mean })
        };
        let mut value = identity.clone();
        value["window"] = json!(window);
        value["buy"] = side(Direction::Buy);
        value["sell"] = side(Direction::Sell);
        println!("HYPOTHESIS_SIDES,{value}");
    }
}

/// Research windows per dataset; each symbol has its own fixed calendar.
fn research_windows_config(symbol: &str) -> &'static str {
    match symbol {
        "XAUUSDT" => include_str!("../../../../config/research-windows.json"),
        "PAXGUSDT" => include_str!("../../../../config/research-windows-paxg.json"),
        "XAUUSD" => include_str!("../../../../config/research-windows-xauusd.json"),
        other => panic!("no research windows for {other}"),
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
    // Plans replay independently, so CI can split one timeframe across jobs.
    let evaluated_plans: Vec<PlanKind> = match std::env::var("GOLD_BASELINE_PLANS") {
        Ok(list) => list
            .split(',')
            .map(|name| {
                PlanKind::ALL
                    .into_iter()
                    .find(|plan| plan.as_str() == name)
                    .unwrap_or_else(|| panic!("unknown plan {name}"))
            })
            .collect(),
        Err(_) => PlanKind::ALL.to_vec(),
    };
    // Diagnostic only: a smaller window shows how much early-history bars differ.
    let raw_window = std::env::var("GOLD_BASELINE_RAW_WINDOW")
        .map(|value| value.parse::<usize>().expect("positive raw window"))
        .unwrap_or(RUNTIME_RAW_WINDOW);
    assert!(raw_window >= 50);
    let symbol =
        Symbol::new(std::env::var("GOLD_BASELINE_SYMBOL").unwrap_or_else(|_| "XAUUSDT".to_owned()))
            .expect("valid baseline symbol");
    // Entry-edge benchmark (edge-v1), printed as EDGE_CASE lines after each case.
    let edge_windows = (std::env::var("GOLD_EDGE_BENCHMARK").as_deref() == Ok("1")).then(|| {
        let dates: ResearchWindowDates =
            serde_json::from_str(research_windows_config(symbol.as_str()))
                .expect("research windows config");
        ResearchWindows::from_dates(&dates).expect("valid research windows")
    });
    let spread_price = std::env::var("GOLD_BASELINE_SPREAD_PRICE")
        .map(|value| value.parse::<Decimal>().expect("decimal spread price"))
        .unwrap_or(Decimal::ZERO);
    assert!(
        edge_windows.is_none() || spread_price.is_zero(),
        "the edge benchmark runs at the baseline default spread of 0"
    );
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
            "cutoff": cutoff, "maxBars": max_bars, "rawWindow": raw_window,
            "historicalArrivalMode": "LIVE_POLICY", "rsiM1EntrySuppressed": true,
            "fees": "0", "extraSlippage": "0", "spread": spread, "executionModel": "OHLC_CONSERVATIVE",
            "databaseWrites": false,
        })
    );

    // The vol-v2 vault check: once, on the laptop, never in CI.
    if std::env::var("GOLD_BASELINE_VOL_VAULT").as_deref() == Ok("1") {
        assert!(
            !running_in_ci(|key| std::env::var(key).ok()),
            "the vault check runs on the laptop only, never in CI"
        );
        assert_eq!(
            symbol.as_str(),
            "XAUUSDT",
            "the vault check is for the live XAUUSDT model"
        );
        assert!(
            evaluated_timeframes == [Timeframe::M15],
            "the live range model is 15m"
        );
        let dates: ResearchWindowDates =
            serde_json::from_str(research_windows_config("XAUUSDT")).expect("XAUUSDT windows");
        let windows = ResearchWindows::from_dates(&dates).expect("valid research windows");
        print_vault_check(
            symbol.as_str(),
            &histories[&Timeframe::M15],
            cutoff,
            max_bars,
            windows.vault_start,
        );
        pool.close().await;
        return;
    }

    // The range forecast benchmark (vol-v1) replaces the plan replay.
    if std::env::var("GOLD_BASELINE_VOLATILITY").as_deref() == Ok("1") {
        let dates: ResearchWindowDates =
            serde_json::from_str(research_windows_config(symbol.as_str()))
                .expect("research windows config");
        let windows = ResearchWindows::from_dates(&dates).expect("valid research windows");
        for timeframe in &evaluated_timeframes {
            print_volatility(
                symbol.as_str(),
                *timeframe,
                &histories[timeframe],
                max_bars,
                &windows,
            );
        }
        pool.close().await;
        return;
    }

    // Research-only hypotheses replace the plan replay; edge-v1 scores their entries.
    if let Ok(list) = std::env::var("GOLD_BASELINE_HYPOTHESES") {
        let windows = edge_windows
            .as_ref()
            .expect("hypotheses are scored only by the edge benchmark");
        for name in list.split(',') {
            let hypothesis =
                Hypothesis::from_name(name).unwrap_or_else(|| panic!("unknown hypothesis {name}"));
            assert!(
                evaluated_timeframes == [hypothesis.timeframe()],
                "{name} is registered for {} only",
                hypothesis.timeframe().as_str()
            );
            print_hypothesis_edge(hypothesis, &histories, max_bars, windows);
        }
        pool.close().await;
        return;
    }

    // Every history stays loaded, because a timeframe reads its higher timeframe's candles.
    for timeframe in evaluated_timeframes {
        let history = &histories[&timeframe];
        let start = history.candles.len().saturating_sub(max_bars);
        let evaluation_candles = &history.candles[start..];
        let registry = runtime_plan_registry();
        let cases: Vec<_> = evaluated_plans
            .iter()
            .copied()
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
        let edge_outcomes = edge_windows.map(|_| {
            let atr = analysis_core::calculate_atr(&history.candles, EDGE_V1.atr_period)
                .expect("edge ATR")
                .values;
            precompute_outcomes(&history.candles, &atr, &EDGE_V1).expect("edge outcomes")
        });
        // Readiness when each bar closed, per case; bars before `start` were never evaluated.
        let mut ready_at_close = vec![vec![false; start]; cases.len()];
        let mut higher_cache: Option<(usize, TimeframeFacts, usize)> = None;
        let mut gap_frames = 0_usize;
        for index in start..history.candles.len() {
            let candle = &history.candles[index];
            let window = fact_window(history, index, raw_window);
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
                    let higher_window = fact_window(higher_history, end, raw_window);
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
            for (case_index, (session, (profile, _, _))) in
                sessions.iter_mut().zip(&cases).enumerate()
            {
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
                ready_at_close[case_index].push(frame.snapshot.readiness == Readiness::Ready);
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
        for (case_index, (session, (profile, _, runtime_algorithm))) in
            sessions.into_iter().zip(&cases).enumerate()
        {
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
            if let (Some(windows), Some(outcomes)) = (&edge_windows, &edge_outcomes) {
                let entries: Vec<_> = result
                    .trades
                    .iter()
                    .map(|trade| RealEntry {
                        entry_time: trade.entry_time,
                        entry_price: trade.entry_price,
                        direction: trade.direction,
                    })
                    .collect();
                let eligible = eligible_bars(
                    &history.candles,
                    outcomes,
                    &ready_at_close[case_index],
                    windows,
                )
                .expect("edge eligibility");
                let case_key = format!(
                    "{}/{}/{}/{}",
                    timeframe.as_str(),
                    profile.key.plan.as_str(),
                    profile.versions.algorithm_version,
                    profile.versions.parameter_version
                );
                for line in evaluate_case(
                    &history.candles,
                    outcomes,
                    &eligible,
                    &entries,
                    windows,
                    &case_key,
                    &EDGE_V1,
                ) {
                    let mut value = serde_json::to_value(&line).expect("edge line");
                    value["timeframe"] = json!(timeframe.as_str());
                    value["plan"] = json!(profile.key.plan.as_str());
                    value["algorithmVersion"] = json!(profile.versions.algorithm_version);
                    value["parameterVersion"] = json!(profile.versions.parameter_version);
                    value["runtimeAlgorithm"] = json!(runtime_algorithm);
                    println!("EDGE_CASE,{value}");
                }
            }
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
    let window = fact_window(&history, history.candles.len() - 1, RUNTIME_RAW_WINDOW);
    assert_eq!(window.len(), 500);
    assert_eq!(window.first().unwrap().open_time, 200 * 60_000);
}

#[test]
fn edge_atr_at_the_signal_bar_ignores_the_entry_bar() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let candle = |index: i64, high: i64, low: i64, close: i64| {
        Candle::new(
            symbol.clone(),
            Timeframe::M5,
            index * 300_000,
            index * 300_000 + 299_999,
            Decimal::new(100, 0),
            Decimal::new(high, 0),
            Decimal::new(low, 0),
            Decimal::new(close, 0),
        )
        .unwrap()
    };
    let mut candles: Vec<_> = (0..30).map(|index| candle(index, 102, 98, 100)).collect();
    let before = analysis_core::calculate_atr(&candles, EDGE_V1.atr_period).unwrap();
    // An entry at bar 20 reads the ATR of bar 19; reshaping bar 20 must not move it.
    candles[20] = candle(20, 150, 60, 140);
    let after = analysis_core::calculate_atr(&candles, EDGE_V1.atr_period).unwrap();
    assert_eq!(before.values[19], after.values[19]);
    assert_ne!(before.values[20], after.values[20]);
}

#[test]
fn paxg_windows_end_where_xauusdt_history_begins() {
    let dates: ResearchWindowDates =
        serde_json::from_str(research_windows_config("PAXGUSDT")).unwrap();
    let windows = ResearchWindows::from_dates(&dates).unwrap();
    // PAXG history stops at the XAUUSDT listing, so the two datasets never overlap.
    assert_eq!(windows.vault_start, 1_765_411_200_000);
    assert!(
        windows.train_start >= 1_609_459_200_000,
        "train never starts before 2021-01"
    );
    assert!(windows.train_start < windows.vault_start);
}

#[test]
fn spot_gold_windows_end_where_xauusdt_history_begins() {
    let dates: ResearchWindowDates =
        serde_json::from_str(research_windows_config("XAUUSD")).unwrap();
    let windows = ResearchWindows::from_dates(&dates).unwrap();
    // Spot history stops at the XAUUSDT listing; the XAUUSDT windows stay untouched.
    assert_eq!(windows.vault_start, 1_765_411_200_000);
    assert_eq!(
        windows.train_start, 1_262_304_000_000,
        "train starts 2010-01-01"
    );
    assert_eq!(
        windows.test_start, 1_640_995_200_000,
        "test starts 2022-01-01"
    );
}

#[test]
fn the_vault_check_refuses_ci_runners() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    };
    assert!(running_in_ci(env(&[("GITHUB_ACTIONS", "true")])));
    assert!(running_in_ci(env(&[("CI", "1")])));
    assert!(!running_in_ci(env(&[])));
    assert!(!running_in_ci(env(&[
        ("CI", "false"),
        ("GITHUB_ACTIONS", "")
    ])));
}

#[test]
fn research_windows_config_is_ordered_and_matches_the_agreed_dates() {
    let dates: ResearchWindowDates =
        serde_json::from_str(include_str!("../../../../config/research-windows.json")).unwrap();
    let windows = ResearchWindows::from_dates(&dates).unwrap();
    assert_eq!(windows.train_start, 1_767_312_000_000);
    // The vault starts right after the dataset cutoff used by CI.
    assert_eq!(windows.vault_start, 1_793_836_799_999 + 1);
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
