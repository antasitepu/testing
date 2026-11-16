use analysis_core::range_forecast::{
    HORIZON_BARS, Lookbacks, RangeModel, har_features, new_york_slot, relative_true_ranges,
    seasonal_feature,
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
        Symbol::new("XAUUSDT").unwrap(),
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

/// `count` hourly candles at 100.00 with a 2.00 range: every relative TR is 0.02.
fn flat_hours(count: i64) -> Vec<Candle> {
    let start = utc(2026, 1, 5, 0, 0);
    (0..count)
        .map(|index| hour_candle(start + index * HOUR, 10_000, 10_100, 9_900, 10_000))
        .collect()
}

fn model() -> RangeModel {
    RangeModel {
        vol_version: "vol-v1".to_owned(),
        model: "HAR_RANGE_SEASONAL".to_owned(),
        symbol: "XAUUSDT".to_owned(),
        timeframe: "1h".to_owned(),
        horizon_bars: HORIZON_BARS,
        coefficients: vec![1.1, 0.5, 0.3, 0.1, 1.0],
        baseline_coefficients: vec![-1.6, 0.5],
        profile: (0..24)
            .map(|hour| if hour == 9 { 2.0 } else { 1.0 })
            .collect(),
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
    let start = utc(2026, 1, 5, 0, 0);
    let candles = vec![
        hour_candle(start, 10_000, 10_100, 9_900, 10_000),
        hour_candle(start + HOUR, 10_350, 10_400, 10_300, 10_350),
    ];
    let rtr = relative_true_ranges(&candles);
    assert_eq!(rtr[0], None);
    assert!((rtr[1].unwrap() - 0.04).abs() < 1e-12);
}

#[test]
fn new_york_slot_follows_daylight_saving() {
    assert_eq!(new_york_slot(utc(2021, 3, 1, 13, 30), Timeframe::M15), 34);
    assert_eq!(new_york_slot(utc(2021, 3, 15, 12, 30), Timeframe::M15), 34);
    assert_eq!(new_york_slot(utc(2021, 7, 1, 21, 0), Timeframe::H1), 17);
}

#[test]
fn har_and_seasonal_features_need_a_month_and_average_the_profile() {
    let candles = flat_hours(529);
    assert!(har_features(&candles[..528], Timeframe::H1).is_none());
    let [ln_rd, ln_rw, ln_rm] = har_features(&candles, Timeframe::H1).unwrap();
    for value in [ln_rd, ln_rw, ln_rm] {
        assert!((value - 0.02_f64.ln()).abs() < 1e-12);
    }

    let profile = model().profile;
    // 14:00 UTC in January is 09:00 in New York, the slot with profile 2.
    let opens: Vec<i64> = (0..24)
        .map(|step| utc(2026, 2, 2, 14, 0) + step * HOUR)
        .collect();
    let ln_season = seasonal_feature(&profile, &opens, Timeframe::H1).unwrap();
    assert!((ln_season - (25.0_f64 / 24.0).ln()).abs() < 1e-12);
    assert_eq!(seasonal_feature(&profile, &[], Timeframe::H1), None);
}

#[test]
fn forecast_applies_the_frozen_coefficients_to_the_last_close() {
    let candles = flat_hours(600);
    let last_open = candles.last().unwrap().open_time;
    let opens: Vec<i64> = (1..=24).map(|step| last_open + step * HOUR).collect();
    let model = model();
    assert_eq!(model.validated_timeframe(), Ok(Timeframe::H1));
    let forecast = model
        .forecast(&candles, Decimal::new(200, 2), &opens, Timeframe::H1)
        .unwrap();
    let ln_season = seasonal_feature(&model.profile, &opens, Timeframe::H1).unwrap();
    let x = 0.02_f64.ln();
    let expected = (1.1 + 0.5 * x + 0.3 * x + 0.1 * x + ln_season).exp() * 100.0;
    assert!((forecast.range - expected).abs() < 1e-9 * expected);
    let expected_atr = (-1.6 + 0.5 * x).exp() * 100.0;
    assert!((forecast.atr_range - expected_atr).abs() < 1e-9 * expected_atr);
    // Exactly one horizon of opens is required.
    assert!(
        model
            .forecast(&candles, Decimal::new(200, 2), &opens[1..], Timeframe::H1)
            .is_none()
    );
}

#[test]
fn malformed_models_are_rejected() {
    let mut short = model();
    short.coefficients.pop();
    assert!(short.validated_timeframe().is_err());
    let mut wrong_slots = model();
    wrong_slots.profile.pop();
    assert!(wrong_slots.validated_timeframe().is_err());
    let mut not_finite = model();
    not_finite.profile[0] = f64::NAN;
    assert!(not_finite.validated_timeframe().is_err());
    let mut other = model();
    other.model = "HAR_RANGE".to_owned();
    assert!(other.validated_timeframe().is_err());
}
