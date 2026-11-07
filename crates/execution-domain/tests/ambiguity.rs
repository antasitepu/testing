use analysis_domain::Direction;
use execution_domain::{
    Ambiguity, ExecutionKind, OhlcExecutionRange, PositionStage, classify_ohlc_ambiguity,
};
use rust_decimal::Decimal;

#[test]
fn lower_policy_classifier_resolves_partial_breakeven_equality_conservatively() {
    let decision = classify_ohlc_ambiguity(OhlcExecutionRange {
        direction: Direction::Buy,
        entry_price: Decimal::new(4_105, 0),
        target: Decimal::new(4_128, 0),
        tp1: Some(Decimal::new(4_120, 0)),
        tp2_edge: Some(Decimal::new(4_128, 0)),
        stop: Decimal::new(4_090, 0),
        trailing_stop: None,
        position_stage: PositionStage::FullPosition,
        entry_low: None,
        entry_high: None,
        low: Decimal::new(4_105, 0),
        high: Decimal::new(4_121, 0),
    });

    assert_eq!(
        decision.actions,
        [ExecutionKind::PartialTarget, ExecutionKind::TrailingStop]
    );
    assert_eq!(decision.ambiguity, Ambiguity::Ambiguous);
}
