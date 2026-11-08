use analysis_core::calculate_atr;
use analysis_core::range_forecast::{HORIZON_BARS, RangeModel};
use gold_monitoring_server::range_forecast::forecast_horizon;
use market_domain::{Candle, MarketSession, SessionStatus, Symbol, Timeframe};
use rust_decimal::Decimal;

const QUARTER: i64 = 900_000;
const DAY: i64 = 86_400_000;
/// Monday 2026-10-05 00:00 UTC.
const START: i64 = 1_791_158_400_000;

fn model() -> RangeModel {
    RangeModel {
        vol_version: "vol-v1".to_owned(),
        model: "HAR_RANGE_SEASONAL".to_owned(),
        symbol: "XAUUSDT".to_owned(),
        timeframe: "15m".to_owned(),
        horizon_bars: HORIZON_BARS,
        coefficients: vec![1.1, 0.5, 0.3, 0.1, 1.0],
        baseline_coefficients: vec![-1.6, 0.5],
        profile: (0..96)
            .map(|slot| 0.5 + f64::from(slot % 7) / 6.0)
            .collect(),
    }
}

/// `count` consecutive 15m candles from `START` with a varying range.
fn quarters(count: i64) -> Vec<Candle> {
    (0..count)
        .map(|index| {
            let half = 100 + (index * 37) % 150;
            let close = 400_000 + (index * 53) % 900;
            Candle::new(
                Symbol::new("XAUUSDT").unwrap(),
                Timeframe::M15,
                START + index * QUARTER,
                START + (index + 1) * QUARTER - 1,
                Decimal::new(close, 2),
                Decimal::new(close + half, 2),
                Decimal::new(close - half, 2),
                Decimal::new(close, 2),
            )
            .unwrap()
        })
        .collect()
}

fn session(status: SessionStatus, start_time: i64, end_time: i64) -> MarketSession {
    MarketSession::new("COMMODITY", status, start_time, end_time, 1).unwrap()
}

/// 30 trading days of history; `now` is 7 minutes into the next bar.
fn ready_case() -> (Vec<Candle>, i64) {
    let candles = quarters(30 * 96);
    let now = START + 30 * DAY + 7 * 60_000;
    (candles, now)
}

#[test]
fn a_ready_history_forecasts_the_next_24_bars_like_the_model() {
    let (candles, now) = ready_case();
    let sessions = [session(SessionStatus::Regular, START, now + 2 * DAY)];
    let forecast = forecast_horizon(
        &model(),
        Timeframe::M15,
        candles.clone(),
        &sessions,
        START,
        now,
    )
    .unwrap();
    let current_open = START + 30 * DAY;
    assert_eq!(forecast.from_time, current_open);
    assert_eq!(forecast.until_time, current_open + 24 * QUARTER - 1);

    let atr = calculate_atr(&candles, 14)
        .unwrap()
        .values
        .last()
        .unwrap()
        .unwrap();
    let opens: Vec<i64> = (0..24).map(|step| current_open + step * QUARTER).collect();
    let direct = model()
        .forecast(&candles, atr, &opens, Timeframe::M15)
        .unwrap();
    assert_eq!(forecast.range, direct.range);
    assert_eq!(forecast.atr_range, direct.atr_range);
    assert!(forecast.range > 0.0);
}

#[test]
fn the_forming_bar_is_ignored_and_closed_days_are_skipped_ahead() {
    let (mut candles, now) = ready_case();
    let current_open = START + 30 * DAY;
    // A forming candle at the current open is not history.
    candles.extend(quarters(30 * 96 + 1).into_iter().skip(30 * 96));
    // The market closes for an hour starting two bars from now.
    let closed_from = current_open + 2 * QUARTER;
    let sessions = [
        session(SessionStatus::Regular, START, closed_from),
        session(
            SessionStatus::NoTrading,
            closed_from,
            closed_from + 4 * QUARTER,
        ),
        session(
            SessionStatus::Regular,
            closed_from + 4 * QUARTER,
            now + 2 * DAY,
        ),
    ];
    let forecast =
        forecast_horizon(&model(), Timeframe::M15, candles, &sessions, START, now).unwrap();
    assert_eq!(forecast.from_time, current_open);
    // 24 tradable bars span 28 quarter hours because of the closed hour.
    assert_eq!(forecast.until_time, current_open + 28 * QUARTER - 1);
}

#[test]
fn missing_bars_short_history_and_unknown_sessions_give_no_forecast() {
    let (candles, now) = ready_case();
    let sessions = [session(SessionStatus::Regular, START, now + 2 * DAY)];

    let mut gapped = candles.clone();
    gapped.remove(gapped.len() - 10);
    assert_eq!(
        forecast_horizon(&model(), Timeframe::M15, gapped, &sessions, START, now),
        Err("recent candles are missing")
    );

    let short: Vec<Candle> = candles[candles.len() - 2000..].to_vec();
    assert_eq!(
        forecast_horizon(&model(), Timeframe::M15, short, &sessions, START, now),
        Err("less than a month of history")
    );

    let ends_now = [session(SessionStatus::Regular, START, now + 3 * QUARTER)];
    assert_eq!(
        forecast_horizon(&model(), Timeframe::M15, candles, &ends_now, START, now),
        Err("upcoming sessions are not known yet")
    );
}

/// Manual check against the local database, read-only:
/// `RANGE_FORECAST_MODEL_PATH=<absolute path> cargo test -p gold-monitoring-server --test range_forecast -- --ignored`
#[tokio::test]
#[ignore = "reads the local database and a private model file"]
async fn live_forecast_from_the_local_database() {
    use gold_monitoring_server::range_forecast::RangeForecaster;
    use market_storage::{CandleRepository, MarketRepository, SessionRepository};
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;

    let _ = dotenvy::dotenv();
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
        .connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .await
        .expect("connect read-only");
    let path = std::env::var("RANGE_FORECAST_MODEL_PATH").expect("RANGE_FORECAST_MODEL_PATH");
    let forecaster = RangeForecaster::from_file(
        std::path::Path::new(&path),
        Arc::new(SessionRepository::new(pool.clone())),
    )
    .expect("model");
    // `RANGE_FORECAST_NOW_MS` replays an earlier moment when the local data stops early.
    let now = std::env::var("RANGE_FORECAST_NOW_MS")
        .map(|value| value.parse::<i64>().expect("epoch milliseconds"))
        .unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        });
    let dto = forecaster
        .forecast(
            &MarketRepository::new(pool.clone()),
            &CandleRepository::new(pool.clone()),
            &Symbol::new("XAUUSDT").unwrap(),
            now,
        )
        .await
        .expect("forecast");
    println!("RANGE_FORECAST,{}", serde_json::to_string(&dto).unwrap());
    assert_eq!(dto.reason, None, "{dto:?}");
}
