use analysis_core::{
    ConfirmedSwing, DivergenceKind, IndicatorOutput, RsiPivot, SwingKind, calculate_atr,
    calculate_rsi, detect_regular_divergence,
};
use analysis_domain::{FactId, FactKind, Readiness, ReadinessReason};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    rsi_closes: Vec<i64>,
    rsi_period: usize,
    expected_rsi: Vec<Option<String>>,
    atr_candles: Vec<[i64; 4]>,
    atr_period: usize,
    expected_atr: Vec<Option<String>>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/momentum.json")).unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn fact_id(kind: FactKind, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M1,
        kind,
        confirmed_at,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-1m-v1".to_owned(),
    }
}

fn decimal_values(values: &[Option<String>]) -> Vec<Option<Decimal>> {
    values
        .iter()
        .map(|value| value.as_deref().map(Decimal::from_str).transpose().unwrap())
        .collect()
}

#[test]
fn wilder_rsi_matches_golden_seed_updates_and_flat_market() {
    let fixture = fixture();
    let closes: Vec<_> = fixture
        .rsi_closes
        .iter()
        .map(|value| Decimal::new(*value, 0))
        .collect();
    let output = calculate_rsi(&closes, fixture.rsi_period).unwrap();
    assert_eq!(output.readiness, Readiness::Ready);
    assert_eq!(output.values, decimal_values(&fixture.expected_rsi));

    let flat = calculate_rsi(&[Decimal::TEN; 5], 3).unwrap();
    assert_eq!(flat.values[3], Some(Decimal::new(50, 0)));

    let warmup = calculate_rsi(&closes[..3], 3).unwrap();
    assert_eq!(
        warmup.readiness,
        Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory
        }
    );
}

#[test]
fn wilder_atr_matches_true_range_golden_values() {
    let fixture = fixture();
    let candles: Vec<_> = fixture
        .atr_candles
        .iter()
        .enumerate()
        .map(|(index, values)| {
            let open_time = i64::try_from(index).unwrap() * 60_000;
            Candle::new(
                symbol(),
                Timeframe::M1,
                open_time,
                open_time + 59_999,
                Decimal::new(values[0], 0),
                Decimal::new(values[1], 0),
                Decimal::new(values[2], 0),
                Decimal::new(values[3], 0),
            )
            .unwrap()
        })
        .collect();
    let output = calculate_atr(&candles, fixture.atr_period).unwrap();
    assert_eq!(output.values, decimal_values(&fixture.expected_atr));
}

fn price_swing(kind: SwingKind, ordinal: u32, price: i64, pivot_open_time: i64) -> ConfirmedSwing {
    ConfirmedSwing {
        id: fact_id(
            match kind {
                SwingKind::High => FactKind::SwingHigh,
                SwingKind::Low => FactKind::SwingLow,
            },
            ordinal,
            pivot_open_time + 60_000,
        ),
        kind,
        label: None,
        price: Decimal::new(price, 0),
        pivot_open_time,
        confirmed_at: pivot_open_time + 60_000,
        source_start_time: pivot_open_time - 60_000,
        source_end_time: pivot_open_time + 60_000,
    }
}

fn rsi_pivot(kind: SwingKind, ordinal: u32, value: i64, pivot_open_time: i64) -> RsiPivot {
    RsiPivot {
        id: fact_id(FactKind::Rsi, ordinal, pivot_open_time + 60_000),
        kind,
        value: Decimal::new(value, 0),
        pivot_open_time,
        confirmed_at: pivot_open_time + 60_000,
    }
}

#[test]
fn regular_divergence_uses_confirmed_matching_pivots_and_distance_bounds() {
    let price = vec![
        price_swing(SwingKind::Low, 1, 100, 60_000),
        price_swing(SwingKind::Low, 2, 90, 300_000),
        price_swing(SwingKind::High, 3, 110, 420_000),
        price_swing(SwingKind::High, 4, 120, 660_000),
    ];
    let rsi = vec![
        rsi_pivot(SwingKind::Low, 1, 20, 60_000),
        rsi_pivot(SwingKind::Low, 2, 30, 300_000),
        rsi_pivot(SwingKind::High, 3, 80, 420_000),
        rsi_pivot(SwingKind::High, 4, 70, 660_000),
    ];
    let divergences =
        detect_regular_divergence(&price, &rsi, 2, 6, "rsi-div-v1", "fixture-v1").unwrap();
    assert_eq!(
        divergences.iter().map(|fact| fact.kind).collect::<Vec<_>>(),
        vec![
            DivergenceKind::RegularBullish,
            DivergenceKind::RegularBearish
        ]
    );

    let none = detect_regular_divergence(&price, &rsi, 5, 6, "rsi-div-v1", "fixture-v1").unwrap();
    assert!(none.is_empty());
}

#[test]
fn indicator_output_type_keeps_readiness_separate_from_values() {
    let output = IndicatorOutput {
        readiness: Readiness::Ready,
        values: vec![Some(Decimal::ONE)],
    };
    assert_eq!(output.values.len(), 1);
}
