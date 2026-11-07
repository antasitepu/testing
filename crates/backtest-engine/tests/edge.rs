use analysis_domain::Direction;
use backtest_engine::edge::{
    BarOutcomes, BracketExit, BracketOutcome, EDGE_V1, EdgeParameters, EdgeStatus, NullVariant,
    RealEntry, ResearchWindow, ResearchWindowDates, ResearchWindows, SplitMix64, eligible_bars,
    evaluate_case, fnv1a64, monte_carlo_p, null_summary_random_time, precompute_outcomes,
    score_entries, simulate_bracket, utc_midnight_ms,
};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;

const BAR: i64 = 300_000;

fn windows() -> ResearchWindows {
    ResearchWindows::from_dates(&ResearchWindowDates {
        version: "research-windows-v1".to_owned(),
        train_start: "2026-01-02".to_owned(),
        validation_start: "2026-07-01".to_owned(),
        test_start: "2026-09-01".to_owned(),
        vault_start: "2026-11-05".to_owned(),
    })
    .unwrap()
}

/// Prices in cents; bar `index` opens at `start + index * BAR`.
fn candle(start: i64, index: usize, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = start + index as i64 * BAR;
    Candle::new(
        Symbol::new("XAUUSDT").unwrap(),
        Timeframe::M5,
        open_time,
        open_time + BAR - 1,
        Decimal::new(open, 2),
        Decimal::new(high, 2),
        Decimal::new(low, 2),
        Decimal::new(close, 2),
    )
    .unwrap()
}

/// 40 quiet bars around 100.00; `shape` overrides bar 1 (the entry bar).
fn quiet(shape: Option<(i64, i64, i64, i64)>) -> Vec<Candle> {
    let start = windows().train_start;
    (0..40)
        .map(|index| match (index, shape) {
            (1, Some((open, high, low, close))) => candle(start, index, open, high, low, close),
            _ => candle(start, index, 10_000, 10_050, 9_950, 10_000),
        })
        .collect()
}

/// ATR 2.00, so the bracket distance is 3.00.
fn atr(candles: &[Candle]) -> Vec<Option<Decimal>> {
    vec![Some(Decimal::new(200, 2)); candles.len()]
}

fn outcome(candles: &[Candle], direction: Direction) -> BracketOutcome {
    simulate_bracket(candles, 1, direction, Decimal::new(200, 2), &EDGE_V1).unwrap()
}

#[test]
fn dates_and_windows_follow_the_research_calendar() {
    assert_eq!(utc_midnight_ms("2026-01-02").unwrap(), 1_767_312_000_000);
    assert_eq!(utc_midnight_ms("1970-01-01").unwrap(), 0);
    assert_eq!(utc_midnight_ms("2024-02-29").unwrap(), 1_709_164_800_000);
    for invalid in [
        "2026-02-29",
        "2026-13-01",
        "2026-1-02",
        "26-01-02",
        "2026-01-02x",
    ] {
        assert!(utc_midnight_ms(invalid).is_err(), "{invalid}");
    }
    let windows = windows();
    assert_eq!(windows.window_of(windows.train_start - 1), None);
    assert_eq!(
        windows.window_of(windows.train_start),
        Some(ResearchWindow::Train)
    );
    assert_eq!(
        windows.window_of(windows.validation_start),
        Some(ResearchWindow::Validation)
    );
    assert_eq!(
        windows.window_of(windows.vault_start - 1),
        Some(ResearchWindow::Test)
    );
    assert_eq!(windows.window_of(windows.vault_start), None);
}

#[test]
fn bracket_scores_target_stop_ambiguity_and_time_stop() {
    let target = outcome(
        &quiet(Some((10_000, 10_350, 9_950, 10_300))),
        Direction::Buy,
    );
    assert_eq!(
        (target.r, target.exit, target.ambiguous),
        (1.0, BracketExit::Target, false)
    );

    let stop = outcome(&quiet(Some((10_000, 10_050, 9_650, 9_700))), Direction::Buy);
    assert_eq!((stop.r, stop.exit), (-1.0, BracketExit::Stop));

    let both = outcome(
        &quiet(Some((10_000, 10_400, 9_600, 10_000))),
        Direction::Buy,
    );
    assert_eq!(
        (both.r, both.exit, both.ambiguous),
        (-1.0, BracketExit::Stop, true)
    );

    let short = outcome(
        &quiet(Some((10_000, 10_050, 9_650, 9_700))),
        Direction::Sell,
    );
    assert_eq!((short.r, short.exit), (1.0, BracketExit::Target));

    // Nothing hits within 24 bars: exit at the open of bar 1 + 24.
    let mut candles = quiet(None);
    let start = windows().train_start;
    candles[25] = candle(start, 25, 10_100, 10_150, 10_050, 10_100);
    let time_stop = outcome(&candles, Direction::Buy);
    assert_eq!(time_stop.exit, BracketExit::TimeStop);
    assert!((time_stop.r - 1.0 / 3.0).abs() < 1e-12);
}

#[test]
fn entry_uses_the_atr_of_the_signal_bar_only() {
    let candles = quiet(Some((10_000, 10_350, 9_950, 10_300)));
    let mut atr = vec![None; candles.len()];
    atr[0] = Some(Decimal::new(200, 2));
    let outcomes = precompute_outcomes(&candles, &atr, &EDGE_V1).unwrap();
    assert_eq!(outcomes.long[1].unwrap().r, 1.0);
    assert!(
        outcomes.long[2].is_none(),
        "bar 2 has no ATR at its signal bar"
    );

    // The entry bar's own ATR (which would know bar 1) never changes its outcome.
    atr[1] = Some(Decimal::new(10_000, 2));
    let changed = precompute_outcomes(&candles, &atr, &EDGE_V1).unwrap();
    assert_eq!(changed.long[1], outcomes.long[1]);

    let full = precompute_outcomes(&candles, &self::atr(&candles), &EDGE_V1).unwrap();
    assert!(full.long[candles.len() - 25].is_some());
    assert!(
        full.long[candles.len() - 24].is_none(),
        "needs 24 bars after entry"
    );
}

#[test]
fn eligibility_needs_readiness_window_and_outcomes() {
    let candles = quiet(None);
    let outcomes = precompute_outcomes(&candles, &atr(&candles), &EDGE_V1).unwrap();
    let mut ready = vec![true; candles.len()];
    ready[4] = false;
    let eligible = eligible_bars(&candles, &outcomes, &ready, &windows()).unwrap();
    assert!(!eligible[0], "no signal bar before bar 0");
    assert!(eligible[1]);
    assert!(!eligible[5], "not ready when its signal bar closed");
    assert!(eligible[6]);
    assert!(
        !eligible[candles.len() - 1],
        "no outcome without 24 following bars"
    );

    let warm_up: Vec<_> = (0..40)
        .map(|index| {
            candle(
                windows().train_start - 50 * BAR,
                index,
                10_000,
                10_050,
                9_950,
                10_000,
            )
        })
        .collect();
    let outcomes = precompute_outcomes(&warm_up, &atr(&warm_up), &EDGE_V1).unwrap();
    let eligible = eligible_bars(&warm_up, &outcomes, &[true; 40], &windows()).unwrap();
    assert!(
        eligible.iter().all(|bar| !bar),
        "warm-up bars are never eligible"
    );
}

#[test]
fn real_entries_score_at_the_open_or_one_bar_late() {
    let candles = quiet(None);
    let outcomes = precompute_outcomes(&candles, &atr(&candles), &EDGE_V1).unwrap();
    let mut ready = vec![true; candles.len()];
    ready[8] = false;
    let eligible = eligible_bars(&candles, &outcomes, &ready, &windows()).unwrap();
    let at = |index: usize, cents: i64| RealEntry {
        entry_time: candles[index].open_time,
        entry_price: Decimal::new(cents, 2),
        direction: Direction::Buy,
    };
    let warm_up = RealEntry {
        entry_time: windows().train_start - BAR,
        entry_price: Decimal::new(10_000, 2),
        direction: Direction::Sell,
    };
    let scored = score_entries(
        &candles,
        &eligible,
        &[
            at(3, 10_000),
            at(5, 10_020),
            at(9, 10_000),
            at(8, 10_020),
            warm_up,
        ],
        &windows(),
    );
    let (window, train) = &scored[0];
    assert_eq!(*window, ResearchWindow::Train);
    assert_eq!(train.scored, vec![(3, Direction::Buy), (6, Direction::Buy)]);
    assert_eq!(train.delayed_limit, 1);
    assert_eq!(
        train.dropped, 2,
        "bar 9 and the delayed bar 8 were not ready"
    );
    assert!(scored[1].1.scored.is_empty() && scored[2].1.scored.is_empty());
}

#[test]
fn p_value_seed_and_sampling_are_deterministic() {
    assert_eq!(monte_carlo_p(0, 100_000), 1.0 / 100_001.0);
    assert_eq!(monte_carlo_p(4, 9), 0.5);
    assert_eq!(fnv1a64(""), 0xCBF2_9CE4_8422_2325);
    assert_eq!(fnv1a64("a"), 0xAF63_DC4C_8601_EC8C);
    let mut first = SplitMix64::new(42);
    let mut second = SplitMix64::new(42);
    let draws: Vec<_> = (0..5).map(|_| first.next_u64()).collect();
    assert_eq!(draws, (0..5).map(|_| second.next_u64()).collect::<Vec<_>>());
    assert!((0..1000).all(|_| first.below(7) < 7));
}

#[test]
fn random_draws_only_use_the_pool() {
    let mut outcomes = BarOutcomes {
        long: vec![None; 10],
        short: vec![None; 10],
    };
    for index in 0..10 {
        let r = if index % 2 == 0 { -1.0 } else { 1.0 };
        let value = BracketOutcome {
            r,
            exit: BracketExit::Stop,
            ambiguous: false,
        };
        outcomes.long[index] = Some(value);
        outcomes.short[index] = Some(value);
    }
    let pool = [0, 2, 4, 6, 8];
    let summary =
        null_summary_random_time(&outcomes, &pool, 3, 2, -1.0, 1_000, &mut SplitMix64::new(7))
            .unwrap();
    assert_eq!(summary.null_mean, -1.0);
    assert_eq!(summary.null_std, 0.0);
    assert_eq!(summary.exceedances, 1_000, "ties count as exceedances");
}

/// Driftless walk: 5 sub-steps of +-0.50 per bar.
fn random_walk(bars: usize, seed: u64) -> Vec<Candle> {
    let mut rng = SplitMix64::new(seed);
    let start = windows().train_start;
    let mut price = 100_000_i64;
    (0..bars)
        .map(|index| {
            let open = price;
            let (mut high, mut low) = (open, open);
            for _ in 0..5 {
                price += if rng.coin() { 50 } else { -50 };
                high = high.max(price);
                low = low.min(price);
            }
            candle(start, index, open, high, low, price)
        })
        .collect()
}

#[test]
fn driftless_null_is_centred_and_an_oracle_reaches_the_minimum_p() {
    let parameters: EdgeParameters = EDGE_V1;
    let candles = random_walk(4_000, 11);
    let atr = vec![Some(Decimal::new(100, 2)); candles.len()];
    let outcomes = precompute_outcomes(&candles, &atr, &parameters).unwrap();
    let eligible =
        eligible_bars(&candles, &outcomes, &vec![true; candles.len()], &windows()).unwrap();
    let pool: Vec<_> = (0..candles.len())
        .filter(|index| eligible[*index])
        .collect();
    let longs: Vec<f64> = pool
        .iter()
        .map(|index| outcomes.long[*index].unwrap().r)
        .collect();
    let mean = longs.iter().sum::<f64>() / longs.len() as f64;
    let sd = (longs.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / longs.len() as f64).sqrt();
    // Overlapping 24-bar outcomes are correlated: count one independent sample per horizon.
    let standard_error = sd / (longs.len() as f64 / 24.0).sqrt();
    let null = null_summary_random_time(
        &outcomes,
        &pool,
        200,
        0,
        0.0,
        2_000,
        &mut SplitMix64::new(3),
    )
    .unwrap();
    assert!(
        null.null_mean.abs() < 3.0 * standard_error,
        "{null:?} se {standard_error}"
    );

    let oracle: Vec<_> = pool
        .iter()
        .step_by(20)
        .map(|index| {
            // Perfect foresight of how the next 24 bars resolve the bracket.
            let direction = if outcomes.long[*index].unwrap().r > 0.0 {
                Direction::Buy
            } else {
                Direction::Sell
            };
            RealEntry {
                entry_time: candles[*index].open_time,
                entry_price: candles[*index].open,
                direction,
            }
        })
        .collect();
    let first = evaluate_case(
        &candles,
        &outcomes,
        &eligible,
        &oracle,
        &windows(),
        "5m/ORACLE/test-v1/test-v1",
        &parameters,
    );
    let train_a = first
        .iter()
        .find(|line| line.window == ResearchWindow::Train && line.variant == NullVariant::A)
        .unwrap();
    assert_eq!(train_a.status, EdgeStatus::Ok);
    assert_eq!(train_a.null.unwrap().p_value, 1.0 / 100_001.0);
    assert_eq!(first.len(), 6);
    let insufficient = first
        .iter()
        .filter(|line| line.status == EdgeStatus::Insufficient)
        .count();
    assert_eq!(insufficient, 4, "validation and test have no entries");

    let second = evaluate_case(
        &candles,
        &outcomes,
        &eligible,
        &oracle,
        &windows(),
        "5m/ORACLE/test-v1/test-v1",
        &parameters,
    );
    assert_eq!(first, second, "same inputs, same lines");
}
