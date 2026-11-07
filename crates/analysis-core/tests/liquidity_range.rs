use analysis_core::{
    ConfirmedSwing, DealingRange, FairValueGapDirection, LiquiditySide, RangeRegion, SwingKind,
    build_dealing_range, detect_fair_value_gaps, detect_liquidity_pools, detect_liquidity_sweeps,
    related_sequence,
};
use analysis_domain::{Direction, FactId, FactKind, FactLifecycle};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    bullish_fvg_candles: Vec<[i64; 4]>,
    bearish_fvg_candles: Vec<[i64; 4]>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/liquidity_range.json")).unwrap()
}

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn candles(values: &[[i64; 4]]) -> Vec<Candle> {
    values
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
        .collect()
}

fn id(kind: FactKind, ordinal: u32) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M1,
        kind,
        confirmed_at: 120_000,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-1m-v1".to_owned(),
    }
}

fn swing(kind: SwingKind, ordinal: u32, price: i64, pivot_open_time: i64) -> ConfirmedSwing {
    ConfirmedSwing {
        id: id(
            match kind {
                SwingKind::High => FactKind::SwingHigh,
                SwingKind::Low => FactKind::SwingLow,
            },
            ordinal,
        ),
        kind,
        label: None,
        price: Decimal::new(price, 0),
        pivot_open_time,
        confirmed_at: 120_000,
        source_start_time: pivot_open_time - 60_000,
        source_end_time: pivot_open_time + 60_000,
    }
}

#[test]
fn bullish_and_bearish_fvg_keep_bounds_while_lifecycle_advances() {
    let fixture = fixture();
    let displacement = id(FactKind::Displacement, 1);
    let bullish = detect_fair_value_gaps(
        &candles(&fixture.bullish_fvg_candles),
        Decimal::ONE,
        Some(displacement.clone()),
        "fvg-v1",
        "fixture-v1",
    )
    .unwrap();
    assert_eq!(bullish.len(), 1);
    assert_eq!(bullish[0].direction, FairValueGapDirection::Bullish);
    assert_eq!(
        (bullish[0].low, bullish[0].high),
        (Decimal::TEN, Decimal::new(12, 0))
    );
    assert_eq!(bullish[0].lifecycle, FactLifecycle::Filled);
    assert_eq!(bullish[0].related_displacement_id, Some(displacement));
    assert_eq!(bullish[0].formation_open_times, [0, 60_000, 120_000]);

    let confirmed = detect_fair_value_gaps(
        &candles(&fixture.bullish_fvg_candles[..3]),
        Decimal::ONE,
        None,
        "fvg-v1",
        "fixture-v1",
    )
    .unwrap();
    assert_eq!(confirmed[0].lifecycle, FactLifecycle::Confirmed);

    let partial = detect_fair_value_gaps(
        &candles(&fixture.bullish_fvg_candles[..4]),
        Decimal::ONE,
        None,
        "fvg-v1",
        "fixture-v1",
    )
    .unwrap();
    assert_eq!(partial[0].lifecycle, FactLifecycle::PartiallyFilled);
    assert_eq!(
        (partial[0].low, partial[0].high),
        (bullish[0].low, bullish[0].high)
    );

    let bearish = detect_fair_value_gaps(
        &candles(&fixture.bearish_fvg_candles),
        Decimal::ONE,
        None,
        "fvg-v1",
        "fixture-v1",
    )
    .unwrap();
    assert_eq!(bearish[0].direction, FairValueGapDirection::Bearish);
}

#[test]
fn equal_high_low_pools_create_one_sweep_with_source_identity() {
    let highs = vec![
        swing(SwingKind::High, 1, 100, 60_000),
        swing(SwingKind::High, 2, 101, 180_000),
    ];
    let lows = vec![
        swing(SwingKind::Low, 3, 90, 240_000),
        swing(SwingKind::Low, 4, 89, 360_000),
    ];
    let mut swings = highs.clone();
    swings.extend(lows);
    let pools =
        detect_liquidity_pools(&swings, Decimal::ONE, "liquidity-v1", "fixture-v1").unwrap();
    assert_eq!(pools.len(), 2);
    assert_eq!(pools[0].side, LiquiditySide::BuySide);
    assert_eq!(
        pools[0].source_swing_ids,
        highs.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
    );

    let sweep_candle = Candle::new(
        symbol(),
        Timeframe::M1,
        420_000,
        479_999,
        Decimal::new(100, 0),
        Decimal::new(103, 0),
        Decimal::new(98, 0),
        Decimal::new(99, 0),
    )
    .unwrap();
    let repeated_sweep = Candle::new(
        symbol(),
        Timeframe::M1,
        480_000,
        539_999,
        Decimal::new(100, 0),
        Decimal::new(103, 0),
        Decimal::new(98, 0),
        Decimal::new(99, 0),
    )
    .unwrap();
    let sweeps = detect_liquidity_sweeps(
        &pools,
        &[sweep_candle, repeated_sweep],
        Decimal::ONE,
        "sweep-v1",
        "fixture-v1",
    )
    .unwrap();
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0].direction, Direction::Sell);
    assert_eq!(sweeps[0].source_pool_id, pools[0].id);
    assert_eq!(sweeps[0].source_swing_ids, pools[0].source_swing_ids);
}

#[test]
fn dealing_range_classifies_discount_equilibrium_and_premium() {
    let low = swing(SwingKind::Low, 1, 90, 60_000);
    let high = swing(SwingKind::High, 2, 110, 180_000);
    let range = build_dealing_range(&low, &high, "range-v1", "fixture-v1").unwrap();
    assert_eq!(
        range,
        DealingRange {
            id: range.id.clone(),
            low_swing_id: low.id,
            high_swing_id: high.id,
            low: Decimal::new(90, 0),
            high: Decimal::new(110, 0),
            equilibrium: Decimal::new(100, 0),
        }
    );
    assert_eq!(range.region(Decimal::new(95, 0)), RangeRegion::Discount);
    assert_eq!(range.region(Decimal::new(100, 0)), RangeRegion::Equilibrium);
    assert_eq!(range.region(Decimal::new(105, 0)), RangeRegion::Premium);
}

#[test]
fn unrelated_displacement_sequence_is_rejected() {
    let displacement = id(FactKind::Displacement, 1);
    let other_displacement = id(FactKind::Displacement, 2);
    let sweep_source = id(FactKind::Displacement, 1);
    assert!(related_sequence(
        Some(&displacement),
        Some(&sweep_source),
        &displacement
    ));
    assert!(!related_sequence(
        Some(&other_displacement),
        Some(&sweep_source),
        &displacement
    ));
}
