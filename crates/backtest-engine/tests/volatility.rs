use backtest_engine::edge::{ResearchWindow, ResearchWindows, SplitMix64};
use backtest_engine::volatility::{
    EventKind, EventModel, HORIZON_BARS, Lookbacks, MIN_SLOT_ORIGINS, MacroEvent, MacroEventsFile,
    Origin, RangeModel, apply_profile, band_report, collect_origins, evaluate, evaluate_events,
    event_fit_before_vault, event_terms, fit_seasonal, har_features, least_squares, new_york_slot,
    quantile, relative_true_ranges, seasonal_feature, seasonal_profile, vault_event_scores,
    vault_scores, vault_trading_days, vault_windows,
};
use chrono::{TimeZone, Utc};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;

const HOUR: i64 = 3_600_000;

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
        .timestamp_millis()
}

/// An hourly candle; prices in cents.
fn hour_candle(open_time: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    Candle::new(
        Symbol::new("XAUUSD").unwrap(),
        Timeframe::H1,
        open_time,
        open_time + HOUR - 1,
        Decimal::new(open, 2),
        Decimal::new(high, 2),
        Decimal::new(low, 2),
        Decimal::new(close, 2),
    )
    .unwrap()
}

/// `count` hourly candles from 2021-01-04, open = close = 100.00, range 2.00.
fn flat_hours(count: usize) -> Vec<Candle> {
    let start = utc(2021, 1, 4, 0, 0);
    (0..count as i64)
        .map(|index| hour_candle(start + index * HOUR, 10_000, 10_100, 9_900, 10_000))
        .collect()
}

fn constant_atr(count: usize) -> Vec<Option<Decimal>> {
    (0..count)
        .map(|index| (index >= 13).then(|| Decimal::new(200, 2)))
        .collect()
}

fn windows_at(candles: &[Candle], validation: usize, test: usize) -> ResearchWindows {
    ResearchWindows {
        train_start: candles[0].open_time,
        validation_start: candles[validation].open_time,
        test_start: candles[test].open_time,
        vault_start: candles[candles.len() - 1].open_time + HOUR,
    }
}

#[test]
fn lookbacks_count_bars_per_day_week_and_month() {
    assert_eq!(
        Lookbacks::of(Timeframe::M15),
        Lookbacks {
            day: 96,
            week: 480,
            month: 2112
        }
    );
    assert_eq!(Lookbacks::of(Timeframe::H1).month, 528);
}

#[test]
fn relative_true_range_includes_the_gap_from_the_previous_close() {
    let start = utc(2021, 1, 4, 0, 0);
    let candles = vec![
        hour_candle(start, 10_000, 10_100, 9_900, 10_000),
        // Gap up: high 104, low 103, previous close 100 -> TR 4.
        hour_candle(start + HOUR, 10_350, 10_400, 10_300, 10_350),
        // Inside bar: TR = high - low = 1 against close 103.5.
        hour_candle(start + 2 * HOUR, 10_350, 10_400, 10_300, 10_350),
    ];
    let rtr = relative_true_ranges(&candles);
    assert_eq!(rtr[0], None);
    assert!((rtr[1].unwrap() - 0.04).abs() < 1e-12);
    assert!((rtr[2].unwrap() - 1.0 / 103.5).abs() < 1e-12);
}

#[test]
fn new_york_slot_follows_daylight_saving() {
    // 08:30 in New York in winter (UTC-5) and in summer (UTC-4).
    assert_eq!(new_york_slot(utc(2021, 3, 1, 13, 30), Timeframe::M15), 34);
    assert_eq!(new_york_slot(utc(2021, 3, 15, 12, 30), Timeframe::M15), 34);
    // 17:00 New York in July.
    assert_eq!(new_york_slot(utc(2021, 7, 1, 21, 0), Timeframe::H1), 17);
    assert_eq!(new_york_slot(utc(2021, 7, 1, 21, 30), Timeframe::M30), 35);
}

fn profile_origin(window: ResearchWindow, slot: usize, ratio: f64) -> Origin {
    Origin {
        index: 0,
        window,
        day: 0,
        target: 0.0,
        x_atr: 0.0,
        ln_rd: 0.0,
        ln_rw: 0.0,
        ln_rm: 0.0,
        slot,
        profile_ratio: ratio,
        ln_season: 0.0,
    }
}

#[test]
fn profile_uses_train_only_and_keeps_thin_slots_neutral() {
    let mut origins = Vec::new();
    origins.extend((0..MIN_SLOT_ORIGINS).map(|_| profile_origin(ResearchWindow::Train, 0, 2.0)));
    origins.extend((0..MIN_SLOT_ORIGINS).map(|_| profile_origin(ResearchWindow::Train, 1, 1.0)));
    origins.extend((0..5).map(|_| profile_origin(ResearchWindow::Train, 2, 10.0)));
    // Validation never shapes the profile.
    origins.extend((0..100).map(|_| profile_origin(ResearchWindow::Validation, 1, 50.0)));
    let profile = seasonal_profile(&origins, 4);
    let overall = (60.0 + 30.0 + 50.0) / 65.0;
    assert!((profile[0] - 2.0 / overall).abs() < 1e-12);
    assert!((profile[1] - 1.0 / overall).abs() < 1e-12);
    assert_eq!(profile[2], 1.0);
    assert_eq!(profile[3], 1.0);
}

#[test]
fn origins_need_a_month_of_history_ready_bars_and_one_window() {
    let count = 720;
    let mut candles = flat_hours(count);
    // A spike inside the first origin's horizon.
    let spike = 540;
    let open_time = candles[spike].open_time;
    candles[spike] = hour_candle(open_time, 10_000, 10_500, 9_900, 10_000);
    let mut ready = vec![true; count];
    ready[600] = false;
    let windows = windows_at(&candles, 650, 690);
    let origins = collect_origins(
        &candles,
        &constant_atr(count),
        &ready,
        &windows,
        Timeframe::H1,
    )
    .unwrap();
    let indices: Vec<usize> = origins.origins.iter().map(|origin| origin.index).collect();

    let first = Lookbacks::of(Timeframe::H1).month + 1;
    assert_eq!(indices[0], first);
    // Bar 600 not ready: no origin whose signal bar (601) or last target bar
    // (577) is 600. In the harness a missing bar also makes the next 200 bars
    // unready, which keeps every target span free of gaps.
    assert!(!indices.contains(&577));
    assert!(!indices.contains(&601));
    assert!(
        [576, 578, 600, 602]
            .iter()
            .all(|index| indices.contains(index))
    );
    // No target crosses into validation at bar 650.
    assert!(indices.contains(&626));
    assert!(!(627..=649).any(|index| indices.contains(&index)));
    assert!(indices.contains(&650));
    // The last origin's horizon ends at the last candle; test ends with the data.
    assert_eq!(*indices.last().unwrap(), count - HORIZON_BARS);
    assert_eq!(origins.skipped, [0, 0, 0]);

    let at = |index: usize| {
        origins
            .origins
            .iter()
            .find(|origin| origin.index == index)
            .unwrap()
    };
    // Range 99.00..105.00 over open 100.00, then the plain 2.00 range.
    assert!((at(first).target - 0.06_f64.ln()).abs() < 1e-12);
    assert!((at(spike + 1).target - 0.02_f64.ln()).abs() < 1e-12);
    assert!((at(first).x_atr - 0.02_f64.ln()).abs() < 1e-12);
    assert_eq!(at(first).window, ResearchWindow::Train);
    assert_eq!(at(650).window, ResearchWindow::Validation);
}

#[test]
fn flat_bars_are_skipped_and_counted() {
    let count = 600;
    let mut candles = flat_hours(count);
    let start = 560;
    for candle in candles.iter_mut().skip(start).take(HORIZON_BARS) {
        *candle = hour_candle(candle.open_time, 10_000, 10_000, 10_000, 10_000);
    }
    let windows = windows_at(&candles, 590, 595);
    let origins = collect_origins(
        &candles,
        &constant_atr(count),
        &vec![true; count],
        &windows,
        Timeframe::H1,
    )
    .unwrap();
    // Only the origin whose whole horizon is flat has a zero range.
    assert_eq!(origins.skipped, [1, 0, 0]);
    assert!(!origins.origins.iter().any(|origin| origin.index == start));
}

#[test]
fn least_squares_recovers_exact_coefficients_and_rejects_singular_systems() {
    let rows: Vec<Vec<f64>> = (0..20)
        .map(|index| {
            let x = f64::from(index);
            vec![1.0, x, (x * 0.37).sin()]
        })
        .collect();
    let targets: Vec<f64> = rows
        .iter()
        .map(|row| 2.0 + 3.0 * row[1] - 0.5 * row[2])
        .collect();
    let fit = least_squares(&rows, &targets).unwrap();
    for (got, want) in fit.iter().zip([2.0, 3.0, -0.5]) {
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
    }
    let singular: Vec<Vec<f64>> = rows
        .iter()
        .map(|row| vec![1.0, row[1], 2.0 * row[1]])
        .collect();
    assert_eq!(least_squares(&singular, &targets), None);
    assert_eq!(least_squares(&rows[..2], &targets[..2]), None);
}

/// Hourly candles whose ranges follow a slow regime and an hour-of-day shape.
fn synthetic_hours(days: usize) -> Vec<Candle> {
    let mut random = SplitMix64::new(7);
    let start = utc(2021, 1, 4, 0, 0);
    let mut close = 180_000_i64;
    (0..(days * 24) as i64)
        .map(|index| {
            let hour = index % 24;
            let regime = 1.0 + 0.5 * ((index / 24) as f64 / 7.0).sin();
            let shape = if (13..17).contains(&hour) { 2.0 } else { 1.0 };
            let noise = 0.5 + (random.next_u64() % 1000) as f64 / 1000.0;
            let half = (100.0 * regime * shape * noise) as i64 + 1;
            let step = (random.next_u64() % 201) as i64 - 100;
            let open = close;
            close = open + step;
            let high = open.max(close) + half;
            let low = open.min(close) - half;
            hour_candle(start + index * HOUR, open, high, low, close)
        })
        .collect()
}

#[test]
fn evaluate_fits_on_train_and_scores_every_window() {
    let candles = synthetic_hours(120);
    let count = candles.len();
    let windows = windows_at(&candles, 24 * 70, 24 * 95);
    let atr = constant_atr(count)
        .into_iter()
        .enumerate()
        .map(|(index, value)| value.map(|_| candles[index].high - candles[index].low))
        .collect::<Vec<_>>();
    let ready = vec![true; count];
    let report = evaluate(&candles, &atr, &ready, &windows, Timeframe::H1).unwrap();

    assert_eq!(report.profile.len(), 24);
    assert_eq!(report.lines.len(), 9);
    let origins = collect_origins(&candles, &atr, &ready, &windows, Timeframe::H1).unwrap();
    for line in &report.lines {
        assert_eq!(line.status, "OK", "{line:?}");
        assert_eq!(line.horizon_bars, HORIZON_BARS);
        let members: Vec<&Origin> = origins
            .origins
            .iter()
            .filter(|origin| origin.window == line.window)
            .collect();
        assert_eq!(line.origins, members.len());
        let expected_columns = match line.model {
            "ATR14_CAL" => 2,
            "HAR_RANGE" => 4,
            _ => 5,
        };
        assert_eq!(line.coefficients.len(), expected_columns);
        if line.role == "baseline" {
            assert_eq!(line.daily_loss_diff, None);
        } else {
            let mut days: Vec<i64> = members.iter().map(|origin| origin.day).collect();
            days.dedup();
            assert_eq!(line.daily_loss_diff.as_ref().unwrap().len(), days.len());
        }
    }
    // Every model's train error is finite and positive.
    let train_mse: Vec<f64> = report
        .lines
        .iter()
        .filter(|line| line.window == ResearchWindow::Train)
        .map(|line| line.mse.unwrap())
        .collect();
    assert!(train_mse.iter().all(|mse| mse.is_finite() && *mse > 0.0));
}

fn display_model(profile: Vec<f64>) -> RangeModel {
    RangeModel {
        vol_version: "vol-v1".to_owned(),
        model: "HAR_RANGE_SEASONAL".to_owned(),
        symbol: "XAUUSDT".to_owned(),
        timeframe: "1h".to_owned(),
        horizon_bars: HORIZON_BARS,
        coefficients: vec![1.1, 0.5, 0.3, 0.1, 1.0],
        baseline_coefficients: vec![-1.6, 0.5],
        profile,
    }
}

#[test]
fn live_features_match_the_research_origins_exactly() {
    let candles = synthetic_hours(120);
    let count = candles.len();
    let windows = windows_at(&candles, 24 * 70, 24 * 95);
    let atr: Vec<Option<Decimal>> = (0..count)
        .map(|index| (index >= 13).then(|| candles[index].high - candles[index].low))
        .collect();
    let mut origins =
        collect_origins(&candles, &atr, &vec![true; count], &windows, Timeframe::H1).unwrap();
    let profile = seasonal_profile(&origins.origins, 24);
    apply_profile(&candles, &mut origins, &profile, Timeframe::H1);
    let model = display_model(profile.clone());
    assert_eq!(model.validated_timeframe(), Ok(Timeframe::H1));

    for origin in origins.origins.iter().step_by(97) {
        let history = &candles[..origin.index];
        let [ln_rd, ln_rw, ln_rm] = har_features(history, Timeframe::H1).unwrap();
        assert!((ln_rd - origin.ln_rd).abs() < 1e-9);
        assert!((ln_rw - origin.ln_rw).abs() < 1e-9);
        assert!((ln_rm - origin.ln_rm).abs() < 1e-9);
        let opens: Vec<i64> = candles[origin.index..origin.index + HORIZON_BARS]
            .iter()
            .map(|candle| candle.open_time)
            .collect();
        let ln_season = seasonal_feature(&profile, &opens, Timeframe::H1).unwrap();
        assert!((ln_season - origin.ln_season).abs() < 1e-12);

        let forecast = model
            .forecast(
                history,
                atr[origin.index - 1].unwrap(),
                &opens,
                Timeframe::H1,
            )
            .unwrap();
        let close = history
            .last()
            .unwrap()
            .close
            .to_string()
            .parse::<f64>()
            .unwrap();
        let expected =
            (1.1 + 0.5 * origin.ln_rd + 0.3 * origin.ln_rw + 0.1 * origin.ln_rm + origin.ln_season)
                .exp()
                * close;
        assert!((forecast.range - expected).abs() < 1e-6 * expected);
        let expected_atr = (-1.6 + 0.5 * origin.x_atr).exp() * close;
        assert!((forecast.atr_range - expected_atr).abs() < 1e-6 * expected_atr);
    }
}

#[test]
fn display_model_rejects_malformed_fits_and_short_inputs() {
    let mut model = display_model(vec![1.0; 24]);
    model.coefficients.pop();
    assert!(model.validated_timeframe().is_err());
    let mut model = display_model(vec![1.0; 23]);
    assert!(model.validated_timeframe().is_err());
    model.profile.push(f64::NAN);
    assert!(model.validated_timeframe().is_err());

    let model = display_model(vec![1.0; 24]);
    let candles = synthetic_hours(30);
    let opens: Vec<i64> = (1..=HORIZON_BARS as i64)
        .map(|step| candles.last().unwrap().open_time + step * HOUR)
        .collect();
    let atr = Decimal::new(100, 2);
    // A month of history is required, and exactly one horizon of opens.
    assert!(har_features(&candles[..528], Timeframe::H1).is_none());
    assert!(
        model
            .forecast(&candles[..528], atr, &opens, Timeframe::H1)
            .is_none()
    );
    assert!(
        model
            .forecast(&candles, atr, &opens[1..], Timeframe::H1)
            .is_none()
    );
    assert!(
        model
            .forecast(&candles, atr, &opens, Timeframe::H1)
            .is_some()
    );
}

#[test]
fn quantile_interpolates_between_order_statistics_like_r_type_7() {
    let values = [4.0, 1.0, 3.0, 2.0];
    assert!((quantile(&values, 0.10).unwrap() - 1.3).abs() < 1e-12);
    assert!((quantile(&values, 0.90).unwrap() - 3.7).abs() < 1e-12);
    assert_eq!(quantile(&values, 0.0), Some(1.0));
    assert_eq!(quantile(&values, 1.0), Some(4.0));
    assert_eq!(quantile(&[], 0.5), None);
    assert_eq!(quantile(&values, 1.5), None);
}

/// 15m candles whose ranges follow a slow regime and an hour-of-day shape. A
/// 24-bar horizon spans six hours here, so the seasonal term varies by origin
/// (on hourly bars without closures every horizon covers each hour once).
fn synthetic_quarters(days: usize) -> Vec<Candle> {
    let mut random = SplitMix64::new(11);
    let start = utc(2021, 1, 4, 0, 0);
    let quarter = HOUR / 4;
    let mut close = 180_000_i64;
    (0..(days * 96) as i64)
        .map(|index| {
            let hour = (index / 4) % 24;
            let regime = 1.0 + 0.5 * ((index / 96) as f64 / 7.0).sin();
            let shape = if (13..17).contains(&hour) { 2.0 } else { 1.0 };
            let noise = 0.5 + (random.next_u64() % 1000) as f64 / 1000.0;
            let half = (60.0 * regime * shape * noise) as i64 + 1;
            let step = (random.next_u64() % 101) as i64 - 50;
            let open = close;
            close = open + step;
            Candle::new(
                Symbol::new("XAUUSD").unwrap(),
                Timeframe::M15,
                start + index * quarter,
                start + (index + 1) * quarter - 1,
                Decimal::new(open, 2),
                Decimal::new(open.max(close) + half, 2),
                Decimal::new(open.min(close) - half, 2),
                Decimal::new(close, 2),
            )
            .unwrap()
        })
        .collect()
}

#[test]
fn band_report_reuses_the_train_fit_and_scores_each_banded_origin_once() {
    // Enough days that every fit sees at least 30 origins per 15m slot.
    let candles = synthetic_quarters(120);
    let count = candles.len();
    let windows = ResearchWindows {
        train_start: candles[0].open_time,
        validation_start: candles[96 * 70].open_time,
        test_start: candles[96 * 95].open_time,
        vault_start: candles[count - 1].open_time + HOUR,
    };
    let atr: Vec<Option<Decimal>> = (0..count)
        .map(|index| (index >= 13).then(|| candles[index].high - candles[index].low))
        .collect();
    let ready = vec![true; count];
    let report = band_report(&candles, &atr, &ready, &windows, Timeframe::M15)
        .unwrap()
        .unwrap();

    // The live fit is exactly the vol-v1 HAR_RANGE_SEASONAL fit on train.
    let evaluated = evaluate(&candles, &atr, &ready, &windows, Timeframe::M15).unwrap();
    let train_fit = evaluated
        .lines
        .iter()
        .find(|line| line.model == "HAR_RANGE_SEASONAL" && line.window == ResearchWindow::Train)
        .unwrap();
    assert_eq!(report.live_fit.coefficients, train_fit.coefficients);
    assert_eq!(report.live_fit.profile, evaluated.profile);

    let origins = collect_origins(&candles, &atr, &ready, &windows, Timeframe::M15)
        .unwrap()
        .origins;
    let banded = origins
        .iter()
        .filter(|origin| candles[origin.index].open_time >= windows.validation_start)
        .count();
    assert_eq!(report.live_band.origins, banded);
    assert_eq!(report.refit_band.origins, banded);
    assert_eq!(report.refit.origins, origins.len());
    assert_eq!(report.live_band.fit, "train");
    assert_eq!(report.refit_band.fit, "refit-leave-one-month-out");
    for band in [&report.live_band, &report.refit_band] {
        assert!(
            band.q10 < 0.0 && band.q90 > 0.0 && band.q10 < band.q90,
            "{band:?}"
        );
    }
    assert_ne!(report.refit.coefficients, report.live_fit.coefficients);
}

#[test]
fn vault_scoring_uses_only_vault_origins_and_the_frozen_model_as_is() {
    let candles = synthetic_quarters(120);
    let count = candles.len();
    let atr: Vec<Option<Decimal>> = (0..count)
        .map(|index| (index >= 13).then(|| candles[index].high - candles[index].low))
        .collect();
    let ready = vec![true; count];
    let vault_start = candles[96 * 100].open_time;

    // A frozen model fitted on everything before the vault.
    let research = ResearchWindows {
        train_start: candles[0].open_time,
        validation_start: vault_start - 2,
        test_start: vault_start - 1,
        vault_start,
    };
    let before = collect_origins(&candles, &atr, &ready, &research, Timeframe::M15)
        .unwrap()
        .origins;
    let members: Vec<usize> = (0..before.len()).collect();
    let fit = fit_seasonal(&candles, &before, &members, Timeframe::M15).unwrap();
    let model = RangeModel {
        vol_version: "vol-v1".to_owned(),
        model: "HAR_RANGE_SEASONAL".to_owned(),
        symbol: "XAUUSDT".to_owned(),
        timeframe: "15m".to_owned(),
        horizon_bars: HORIZON_BARS,
        coefficients: fit.coefficients.clone(),
        baseline_coefficients: fit.baseline_coefficients.clone(),
        profile: fit.profile.clone(),
    };

    let vault = collect_origins(
        &candles,
        &atr,
        &ready,
        &vault_windows(vault_start),
        Timeframe::M15,
    )
    .unwrap()
    .origins;
    assert!(!vault.is_empty());
    assert!(
        vault
            .iter()
            .all(|origin| candles[origin.index].open_time >= vault_start)
    );
    // The last origin's target ends at the last loaded candle.
    assert_eq!(vault.last().unwrap().index, count - HORIZON_BARS);

    let scores = vault_scores(&candles, &vault, &model, Timeframe::M15);
    assert_eq!(scores.origins, vault.len());
    let origin = &vault[vault.len() / 2];
    let opens: Vec<i64> = candles[origin.index..origin.index + HORIZON_BARS]
        .iter()
        .map(|candle| candle.open_time)
        .collect();
    let ln_season = seasonal_feature(&model.profile, &opens, Timeframe::M15).unwrap();
    let c = &model.coefficients;
    let forecast =
        c[0] + c[1] * origin.ln_rd + c[2] * origin.ln_rw + c[3] * origin.ln_rm + c[4] * ln_season;
    let b = &model.baseline_coefficients;
    let position = vault.len() / 2;
    assert!((scores.residuals[position] - (origin.target - forecast)).abs() < 1e-12);
    assert!(
        (scores.baseline_residuals[position] - (origin.target - (b[0] + b[1] * origin.x_atr)))
            .abs()
            < 1e-12
    );
    assert_eq!(scores.open_times[position], candles[origin.index].open_time);
}

#[test]
fn vault_trading_days_count_dates_inside_the_vault_and_cutoff() {
    let candles = synthetic_quarters(10);
    let vault_start = candles[96 * 4].open_time;
    let cutoff = candles[96 * 7 - 1].close_time;
    assert_eq!(vault_trading_days(&candles, vault_start, cutoff), 3);
    assert_eq!(vault_trading_days(&candles, vault_start, vault_start), 0);
}

fn calendar(rows: &[(&str, &str, &str)]) -> MacroEventsFile {
    let events: Vec<serde_json::Value> = rows
        .iter()
        .map(|(kind, start, end)| serde_json::json!({ "type": kind, "start": start, "end": end }))
        .collect();
    serde_json::from_value(serde_json::json!({ "version": "test", "events": events })).unwrap()
}

#[test]
fn macro_events_parse_with_offsets_sort_and_reject_bad_rows() {
    let file = calendar(&[
        (
            "FOMC",
            "2026-01-28T12:30:00-05:00",
            "2026-01-28T14:15:00-05:00",
        ),
        (
            "NFP",
            "2026-01-09T08:30:00-05:00",
            "2026-01-09T08:30:00-05:00",
        ),
        (
            "CPI",
            "2026-07-14T08:30:00-04:00",
            "2026-07-14T08:30:00-04:00",
        ),
    ]);
    let events = file.events().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![EventKind::Nfp, EventKind::Fomc, EventKind::Cpi]
    );
    assert_eq!(events[0].start, utc(2026, 1, 9, 13, 30));
    assert_eq!(events[2].start, utc(2026, 7, 14, 12, 30));
    assert_eq!(events[1].end - events[1].start, 105 * 60_000);

    assert!(
        calendar(&[(
            "GDP",
            "2026-01-09T08:30:00-05:00",
            "2026-01-09T08:30:00-05:00"
        )])
        .events()
        .is_err()
    );
    assert!(
        calendar(&[(
            "FOMC",
            "2026-01-28T14:15:00-05:00",
            "2026-01-28T12:30:00-05:00"
        )])
        .events()
        .is_err()
    );
}

#[test]
fn event_terms_cover_the_whole_horizon_and_no_more() {
    let candles = flat_hours(600);
    let origin = |index: usize| Origin {
        index,
        window: ResearchWindow::Train,
        day: 0,
        target: 0.0,
        x_atr: 0.0,
        ln_rd: 0.0,
        ln_rw: 0.0,
        ln_rm: 0.0,
        slot: 0,
        profile_ratio: 0.0,
        ln_season: 0.0,
    };
    let from = candles[550].open_time;
    let to = candles[550 + HORIZON_BARS - 1].close_time;
    let point = |kind, time| MacroEvent {
        kind,
        start: time,
        end: time,
    };
    let at = |events: &[MacroEvent]| event_terms(&candles, &origin(550), events);
    assert_eq!(at(&[point(EventKind::Nfp, from)]), [1.0, 0.0, 0.0]);
    assert_eq!(at(&[point(EventKind::Cpi, to)]), [0.0, 1.0, 0.0]);
    assert_eq!(at(&[point(EventKind::Nfp, to + 1)]), [0.0, 0.0, 0.0]);
    assert_eq!(at(&[point(EventKind::Nfp, from - 1)]), [0.0, 0.0, 0.0]);
    // An FOMC interval that started before the horizon still touches it.
    let fomc = MacroEvent {
        kind: EventKind::Fomc,
        start: from - HOUR,
        end: from + 1,
    };
    assert_eq!(at(&[fomc]), [0.0, 0.0, 1.0]);
    let earlier = MacroEvent {
        kind: EventKind::Fomc,
        start: from - 2 * HOUR,
        end: from - 1,
    };
    assert_eq!(
        at(&[earlier, point(EventKind::Nfp, from + HOUR)]),
        [1.0, 0.0, 0.0]
    );
}

#[test]
fn evaluate_events_keeps_the_vol_v1_baseline_and_splits_event_horizons() {
    let candles = synthetic_quarters(120);
    let count = candles.len();
    let windows = ResearchWindows {
        train_start: candles[0].open_time,
        validation_start: candles[96 * 70].open_time,
        test_start: candles[96 * 95].open_time,
        vault_start: candles[count - 1].open_time + HOUR,
    };
    let atr: Vec<Option<Decimal>> = (0..count)
        .map(|index| (index >= 13).then(|| candles[index].high - candles[index].low))
        .collect();
    let ready = vec![true; count];
    // Weekly "NFP" and "CPI" at 13:30 UTC on different days, an FOMC interval every
    // four weeks: every kind occurs in train, so the candidate can be fitted.
    let mut events = Vec::new();
    for (kind, offset) in [(EventKind::Nfp, 3), (EventKind::Cpi, 5)] {
        for day in (offset..120).step_by(7) {
            let time = candles[96 * day].open_time + 13 * HOUR + 30 * 60_000;
            events.push(MacroEvent {
                kind,
                start: time,
                end: time,
            });
        }
    }
    for day in (10..120).step_by(28) {
        let start = candles[96 * day].open_time + 17 * HOUR;
        events.push(MacroEvent {
            kind: EventKind::Fomc,
            start,
            end: start + 105 * 60_000,
        });
    }
    events.sort_by_key(|event| event.start);

    let lines = evaluate_events(&candles, &atr, &ready, &windows, Timeframe::M15, &events).unwrap();
    let vol_v1 = evaluate(&candles, &atr, &ready, &windows, Timeframe::M15).unwrap();
    let seasonal_train = vol_v1
        .lines
        .iter()
        .find(|line| line.model == "HAR_RANGE_SEASONAL" && line.window == ResearchWindow::Train)
        .unwrap();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert_eq!(line.status, "OK", "{line:?}");
        assert_eq!(line.baseline_coefficients, seasonal_train.coefficients);
        assert_eq!(line.coefficients.len(), 8);
        let same_window = vol_v1
            .lines
            .iter()
            .find(|other| other.window == line.window && other.model == "HAR_RANGE_SEASONAL")
            .unwrap();
        assert_eq!(line.origins_event + line.origins_other, same_window.origins);
        assert!(line.origins_event > 0);
        // Every kind occurs in train; later windows need not hold all of them.
        if line.window == ResearchWindow::Train {
            assert!(
                line.horizons_by_kind.iter().all(|count| *count > 0),
                "{line:?}"
            );
        }
        assert!(!line.daily_loss_diff_event.is_empty());
    }
}

fn synthetic_calendar(candles: &[Candle]) -> Vec<MacroEvent> {
    let mut events = Vec::new();
    for (kind, offset) in [(EventKind::Nfp, 3), (EventKind::Cpi, 5)] {
        for day in (offset..candles.len() / 96).step_by(7) {
            let time = candles[96 * day].open_time + 13 * HOUR + 30 * 60_000;
            events.push(MacroEvent {
                kind,
                start: time,
                end: time,
            });
        }
    }
    for day in (10..candles.len() / 96).step_by(28) {
        let start = candles[96 * day].open_time + 17 * HOUR;
        events.push(MacroEvent {
            kind: EventKind::Fomc,
            start,
            end: start + 105 * 60_000,
        });
    }
    events.sort_by_key(|event| event.start);
    events
}

#[test]
fn the_pre_vault_event_fit_shares_its_seasonal_part_with_the_vol_v2_refit() {
    let candles = synthetic_quarters(120);
    let count = candles.len();
    let windows = ResearchWindows {
        train_start: candles[0].open_time,
        validation_start: candles[96 * 70].open_time,
        test_start: candles[96 * 95].open_time,
        vault_start: candles[count - 1].open_time + HOUR,
    };
    let atr: Vec<Option<Decimal>> = (0..count)
        .map(|index| (index >= 13).then(|| candles[index].high - candles[index].low))
        .collect();
    let ready = vec![true; count];
    let events = synthetic_calendar(&candles);
    let (seasonal, coefficients) =
        event_fit_before_vault(&candles, &atr, &ready, &windows, Timeframe::M15, &events)
            .unwrap()
            .unwrap();
    let refit = band_report(&candles, &atr, &ready, &windows, Timeframe::M15)
        .unwrap()
        .unwrap()
        .refit;
    assert_eq!(seasonal, refit);
    assert_eq!(coefficients.len(), 8);

    // Scored on the last stretch as if it were the vault, exactly as frozen.
    let model = EventModel {
        vol_version: "vol-v3".to_owned(),
        symbol: "XAUUSDT".to_owned(),
        timeframe: "15m".to_owned(),
        horizon_bars: HORIZON_BARS,
        calendar_version: "test".to_owned(),
        coefficients: coefficients.clone(),
        seasonal_coefficients: seasonal.coefficients.clone(),
        profile: seasonal.profile.clone(),
        origins: seasonal.origins,
    };
    let vault_start = candles[96 * 100].open_time;
    let vault = collect_origins(
        &candles,
        &atr,
        &ready,
        &vault_windows(vault_start),
        Timeframe::M15,
    )
    .unwrap()
    .origins;
    let scores = vault_event_scores(&candles, &vault, &model, &events, Timeframe::M15);
    assert_eq!(scores.origins, vault.len());
    let position = scores
        .event_horizon
        .iter()
        .position(|flag| *flag)
        .expect("an event horizon in the vault stretch");
    let origin = &vault[position];
    let opens: Vec<i64> = candles[origin.index..origin.index + HORIZON_BARS]
        .iter()
        .map(|candle| candle.open_time)
        .collect();
    let ln_season = seasonal_feature(&model.profile, &opens, Timeframe::M15).unwrap();
    let terms = event_terms(&candles, origin, &events);
    let row = [
        1.0,
        origin.ln_rd,
        origin.ln_rw,
        origin.ln_rm,
        ln_season,
        terms[0],
        terms[1],
        terms[2],
    ];
    let forecast: f64 = row.iter().zip(&coefficients).map(|(v, w)| v * w).sum();
    assert!((scores.residuals[position] - (origin.target - forecast)).abs() < 1e-12);
    let baseline: f64 = row[..5]
        .iter()
        .zip(&seasonal.coefficients)
        .map(|(v, w)| v * w)
        .sum();
    assert!((scores.baseline_residuals[position] - (origin.target - baseline)).abs() < 1e-12);
}
