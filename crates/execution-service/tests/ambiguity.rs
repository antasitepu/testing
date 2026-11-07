use analysis_domain::Direction;
use execution_domain::{Ambiguity, PositionStage};
use execution_service::{
    AmbiguousOutcome, ExecutionKind, OhlcExecutionRange, classify_ohlc_ambiguity,
    known_ordered_outcome,
};
use rust_decimal::Decimal;

#[test]
fn continuous_ordered_events_keep_single_known_outcome_without_ambiguity() {
    let decision = known_ordered_outcome(ExecutionKind::Target);

    assert_eq!(
        decision.outcome,
        AmbiguousOutcome::SingleKnown(ExecutionKind::Target)
    );
    assert_eq!(decision.ambiguity, Ambiguity::None);
}

#[test]
fn reconciliation_candle_with_target_and_stop_possible_chooses_stop_first() {
    let decision = classify_ohlc_ambiguity(range(Direction::Buy, 4_100, 4_130, 4_090, None));

    assert_eq!(decision.outcome, AmbiguousOutcome::StopFirst);
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
    assert_eq!(decision.evidence_low, Decimal::new(4_085, 0));
    assert_eq!(decision.evidence_high, Decimal::new(4_135, 0));
}

#[test]
fn entry_and_target_possible_processes_entry_then_target() {
    let mut range = range(Direction::Buy, 4_100, 4_130, 4_090, None);
    range.entry_low = Some(Decimal::new(4_100, 0));
    range.entry_high = Some(Decimal::new(4_110, 0));
    range.low = Decimal::new(4_105, 0);
    range.high = Decimal::new(4_135, 0);

    let decision = classify_ohlc_ambiguity(range);

    assert_eq!(decision.outcome, AmbiguousOutcome::EntryThenTarget);
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn target_only_and_stop_only_are_single_known_outcomes() {
    let target = classify_ohlc_ambiguity(range(Direction::Buy, 4_100, 4_130, 4_090, None));
    assert_eq!(target.outcome, AmbiguousOutcome::StopFirst);

    let mut target_only = range(Direction::Buy, 4_100, 4_130, 4_090, None);
    target_only.low = Decimal::new(4_100, 0);
    assert_eq!(
        classify_ohlc_ambiguity(target_only).outcome,
        AmbiguousOutcome::SingleKnown(ExecutionKind::Target)
    );

    let mut stop_only = range(Direction::Buy, 4_100, 4_130, 4_090, None);
    stop_only.high = Decimal::new(4_120, 0);
    assert_eq!(
        classify_ohlc_ambiguity(stop_only).outcome,
        AmbiguousOutcome::SingleKnown(ExecutionKind::Stop)
    );
}

#[test]
fn trailing_stop_and_target_possible_choose_stop_first() {
    let mut range = range(Direction::Buy, 4_100, 4_130, 4_090, Some(4_110));
    range.low = Decimal::new(4_105, 0);
    range.high = Decimal::new(4_135, 0);

    let decision = classify_ohlc_ambiguity(range);

    assert_eq!(decision.outcome, AmbiguousOutcome::StopFirst);
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn partial_exit_matrix_returns_conservative_ordered_actions() {
    let mut hard_stop_and_tp1 = range(Direction::Buy, 4_105, 4_128, 4_090, None);
    hard_stop_and_tp1.tp1 = Some(Decimal::new(4_120, 0));
    hard_stop_and_tp1.position_stage = PositionStage::FullPosition;
    let decision = classify_ohlc_ambiguity(hard_stop_and_tp1);
    assert_eq!(decision.actions, [ExecutionKind::Stop]);
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);

    let mut tp1_then_tp2 = range(Direction::Buy, 4_105, 4_128, 4_090, None);
    tp1_then_tp2.tp1 = Some(Decimal::new(4_120, 0));
    tp1_then_tp2.position_stage = PositionStage::FullPosition;
    tp1_then_tp2.low = Decimal::new(4_110, 0);
    let decision = classify_ohlc_ambiguity(tp1_then_tp2);
    assert_eq!(
        decision.actions,
        [ExecutionKind::PartialTarget, ExecutionKind::Target]
    );
    assert_eq!(decision.ambiguity, Ambiguity::None);

    let mut tp1_then_breakeven = range(Direction::Buy, 4_105, 4_128, 4_090, None);
    tp1_then_breakeven.tp1 = Some(Decimal::new(4_120, 0));
    tp1_then_breakeven.position_stage = PositionStage::FullPosition;
    tp1_then_breakeven.low = Decimal::new(4_100, 0);
    tp1_then_breakeven.high = Decimal::new(4_121, 0);
    let decision = classify_ohlc_ambiguity(tp1_then_breakeven);
    assert_eq!(
        decision.actions,
        [ExecutionKind::PartialTarget, ExecutionKind::TrailingStop]
    );
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
}

#[test]
fn partial_target_and_breakeven_equality_is_conservative_for_buy_and_sell() {
    for (direction, entry, tp1, target, stop, low, high) in [
        (Direction::Buy, 4_105, 4_120, 4_128, 4_090, 4_105, 4_121),
        (Direction::Sell, 4_105, 4_090, 4_082, 4_120, 4_089, 4_105),
    ] {
        let mut equality = range(direction, entry, target, stop, None);
        equality.tp1 = Some(Decimal::new(tp1, 0));
        equality.position_stage = PositionStage::FullPosition;
        equality.low = Decimal::new(low, 0);
        equality.high = Decimal::new(high, 0);

        let decision = classify_ohlc_ambiguity(equality);

        assert_eq!(
            decision.actions,
            [ExecutionKind::PartialTarget, ExecutionKind::TrailingStop]
        );
        assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
    }
}

fn range(
    direction: Direction,
    entry: i64,
    target: i64,
    stop: i64,
    trailing_stop: Option<i64>,
) -> OhlcExecutionRange {
    OhlcExecutionRange {
        direction,
        entry_price: Decimal::new(entry, 0),
        target: Decimal::new(target, 0),
        tp1: None,
        tp2_edge: None,
        stop: Decimal::new(stop, 0),
        trailing_stop: trailing_stop.map(|value| Decimal::new(value, 0)),
        position_stage: PositionStage::Legacy,
        entry_low: None,
        entry_high: None,
        low: Decimal::new(4_085, 0),
        high: Decimal::new(4_135, 0),
    }
}
