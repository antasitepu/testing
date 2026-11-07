use analysis_core::{
    DisplacementParameters, StructureEventKind, SwingKind, SwingLabel, SwingParameters,
    calculate_atr, detect_displacements, detect_displacements_with_atr, detect_structure,
};
use analysis_domain::Direction;
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    candles: Vec<[i64; 4]>,
    expected_break_directions: Vec<String>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/structure.json")).unwrap()
}

fn candles(values: &[[i64; 4]]) -> Vec<Candle> {
    values
        .iter()
        .enumerate()
        .map(|(index, values)| {
            let open_time = i64::try_from(index).unwrap() * 60_000;
            Candle::new(
                Symbol::new("XAUUSDT").unwrap(),
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

fn swings() -> SwingParameters {
    SwingParameters {
        left_bars: 1,
        right_bars: 1,
    }
}

#[test]
fn pivot_is_visible_only_after_right_side_closes_and_labels_are_stable() {
    let fixture = fixture();
    let candles = candles(&fixture.candles);
    let before_confirmation =
        detect_structure(&candles[..2], swings(), "structure-v1", "fixture-v1").unwrap();
    assert!(before_confirmation.swings.is_empty());

    let confirmed =
        detect_structure(&candles[..3], swings(), "structure-v1", "fixture-v1").unwrap();
    assert_eq!(confirmed.swings.len(), 1);
    assert_eq!(confirmed.swings[0].kind, SwingKind::High);
    assert_eq!(confirmed.swings[0].confirmed_at, candles[2].close_time);

    let complete = detect_structure(&candles, swings(), "structure-v1", "fixture-v1").unwrap();
    assert!(complete.swings.iter().any(|swing| {
        swing.kind == SwingKind::High && swing.label == Some(SwingLabel::HigherHigh)
    }));
    assert!(complete.swings.iter().any(|swing| {
        swing.kind == SwingKind::Low && swing.label == Some(SwingLabel::LowerLow)
    }));
}

#[test]
fn structure_breaks_are_ordered_and_future_append_does_not_repaint() {
    let fixture = fixture();
    let mut candles = candles(&fixture.candles);
    let baseline = detect_structure(&candles, swings(), "structure-v1", "fixture-v1").unwrap();
    assert_eq!(
        baseline
            .events
            .iter()
            .map(|event| event.direction.as_str())
            .collect::<Vec<_>>(),
        fixture
            .expected_break_directions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        baseline.events[0].kind,
        StructureEventKind::BreakOfStructure
    );
    assert_eq!(
        baseline.events[1].kind,
        StructureEventKind::ChangeOfCharacter
    );

    candles.push(
        Candle::new(
            Symbol::new("XAUUSDT").unwrap(),
            Timeframe::M1,
            540_000,
            599_999,
            Decimal::new(8, 0),
            Decimal::new(10, 0),
            Decimal::new(7, 0),
            Decimal::new(9, 0),
        )
        .unwrap(),
    );
    let replayed = detect_structure(&candles, swings(), "structure-v1", "fixture-v1").unwrap();
    assert_eq!(
        &replayed.events[..baseline.events.len()],
        baseline.events.as_slice()
    );
    assert_eq!(
        &replayed.swings[..baseline.swings.len()],
        baseline.swings.as_slice()
    );
}

#[test]
fn displacement_uses_body_and_range_atr_thresholds() {
    let fixture = fixture();
    let candles = candles(&fixture.candles);
    let displacements = detect_displacements(
        &candles,
        Decimal::ONE,
        DisplacementParameters {
            minimum_body_atr: Decimal::new(2, 0),
            minimum_range_atr: Decimal::new(3, 0),
        },
        "displacement-v1",
        "fixture-v1",
    )
    .unwrap();
    assert!(
        displacements
            .iter()
            .any(|fact| { fact.candle_open_time == 300_000 && fact.direction == Direction::Buy })
    );
    assert!(
        displacements
            .iter()
            .any(|fact| { fact.candle_open_time == 420_000 && fact.direction == Direction::Sell })
    );
}

#[test]
fn equal_open_times_are_rejected_instead_of_reordered() {
    let fixture = fixture();
    let mut candles = candles(&fixture.candles[..3]);
    candles[2] = candles[1].clone();
    assert!(detect_structure(&candles, swings(), "structure-v1", "fixture-v1").is_err());
}

#[test]
fn per_candle_atr_displacements_do_not_repaint_when_a_volatile_candle_arrives() {
    let symbol = Symbol::new("XAUUSDT").unwrap();
    let minute = Timeframe::M1.duration_ms();
    let bar = |index: i64, body: i64, range: i64| {
        let open = 410_000;
        Candle::new(
            symbol.clone(),
            Timeframe::M1,
            index * minute,
            index * minute + minute - 1,
            Decimal::new(open, 2),
            Decimal::new(open + range, 2),
            Decimal::new(open, 2),
            Decimal::new(open + body, 2),
        )
        .unwrap()
    };
    let mut candles: Vec<_> = (0..60)
        .map(|index| {
            if index % 3 == 0 {
                bar(index, 180, 200)
            } else {
                bar(index, 20, 100)
            }
        })
        .collect();
    let parameters = DisplacementParameters {
        minimum_body_atr: Decimal::new(8, 1),
        minimum_range_atr: Decimal::ONE,
    };
    let detect = |candles: &[Candle]| {
        let atr = calculate_atr(candles, 14).unwrap().values;
        detect_displacements_with_atr(candles, &atr, parameters, "displacement-v1", "test-v1")
            .unwrap()
            .into_iter()
            .map(|fact| fact.candle_open_time)
            .collect::<Vec<_>>()
    };
    let before = detect(&candles);
    assert!(!before.is_empty());

    candles.push(bar(60, 5_000, 5_000));
    let after = detect(&candles);

    assert_eq!(&after[..before.len()], before.as_slice());
}
