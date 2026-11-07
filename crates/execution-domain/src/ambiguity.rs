use crate::{Ambiguity, PositionStage};
use analysis_domain::Direction;
use rust_decimal::Decimal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionKind {
    Entry,
    PartialTarget,
    Target,
    Stop,
    TrailingStop,
    NoExecution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AmbiguousOutcome {
    StopFirst,
    EntryThenTarget,
    PartialThenTarget,
    PartialThenStop,
    SingleKnown(ExecutionKind),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AmbiguityDecision {
    pub outcome: AmbiguousOutcome,
    pub actions: Vec<ExecutionKind>,
    pub ambiguity: Ambiguity,
    pub evidence_low: Decimal,
    pub evidence_high: Decimal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OhlcExecutionRange {
    pub direction: Direction,
    pub entry_price: Decimal,
    pub target: Decimal,
    pub tp1: Option<Decimal>,
    pub tp2_edge: Option<Decimal>,
    pub stop: Decimal,
    pub trailing_stop: Option<Decimal>,
    pub position_stage: PositionStage,
    pub entry_low: Option<Decimal>,
    pub entry_high: Option<Decimal>,
    pub low: Decimal,
    pub high: Decimal,
}

pub fn known_ordered_outcome(kind: ExecutionKind) -> AmbiguityDecision {
    AmbiguityDecision {
        outcome: AmbiguousOutcome::SingleKnown(kind),
        actions: vec![kind],
        ambiguity: Ambiguity::None,
        evidence_low: Decimal::ZERO,
        evidence_high: Decimal::ZERO,
    }
}

pub fn classify_ohlc_ambiguity(range: OhlcExecutionRange) -> AmbiguityDecision {
    let target = touches_target(&range);
    let stop = touches_stop(&range);
    let trailing_stop = touches_trailing_stop(&range);
    let entry = touches_entry(&range);
    let partial_target = touches_partial_target(&range);
    let prospective_breakeven = partial_target && crosses_prospective_breakeven(&range);
    let (outcome, actions, ambiguity) =
        if range.position_stage == PositionStage::FullPosition && range.tp1.is_some() {
            if stop {
                let ambiguous = partial_target || target || entry;
                (
                    if ambiguous {
                        AmbiguousOutcome::StopFirst
                    } else {
                        AmbiguousOutcome::SingleKnown(ExecutionKind::Stop)
                    },
                    vec![ExecutionKind::Stop],
                    if ambiguous {
                        Ambiguity::Ambiguous
                    } else {
                        Ambiguity::None
                    },
                )
            } else if partial_target && prospective_breakeven {
                (
                    AmbiguousOutcome::PartialThenStop,
                    vec![ExecutionKind::PartialTarget, ExecutionKind::TrailingStop],
                    Ambiguity::Ambiguous,
                )
            } else if partial_target && target {
                (
                    AmbiguousOutcome::PartialThenTarget,
                    vec![ExecutionKind::PartialTarget, ExecutionKind::Target],
                    Ambiguity::None,
                )
            } else if partial_target {
                (
                    AmbiguousOutcome::SingleKnown(ExecutionKind::PartialTarget),
                    vec![ExecutionKind::PartialTarget],
                    Ambiguity::None,
                )
            } else {
                (
                    AmbiguousOutcome::SingleKnown(ExecutionKind::NoExecution),
                    vec![ExecutionKind::NoExecution],
                    Ambiguity::None,
                )
            }
        } else if stop || trailing_stop {
            let kind = if trailing_stop {
                ExecutionKind::TrailingStop
            } else {
                ExecutionKind::Stop
            };
            let ambiguous = target || entry;
            (
                if ambiguous {
                    AmbiguousOutcome::StopFirst
                } else {
                    AmbiguousOutcome::SingleKnown(kind)
                },
                vec![kind],
                if ambiguous {
                    Ambiguity::Ambiguous
                } else {
                    Ambiguity::None
                },
            )
        } else if entry && target {
            (
                AmbiguousOutcome::EntryThenTarget,
                vec![ExecutionKind::Entry, ExecutionKind::Target],
                Ambiguity::Ambiguous,
            )
        } else if target {
            (
                AmbiguousOutcome::SingleKnown(ExecutionKind::Target),
                vec![ExecutionKind::Target],
                Ambiguity::None,
            )
        } else if entry {
            (
                AmbiguousOutcome::SingleKnown(ExecutionKind::Entry),
                vec![ExecutionKind::Entry],
                Ambiguity::None,
            )
        } else {
            (
                AmbiguousOutcome::SingleKnown(ExecutionKind::NoExecution),
                vec![ExecutionKind::NoExecution],
                Ambiguity::None,
            )
        };
    AmbiguityDecision {
        outcome,
        actions,
        ambiguity,
        evidence_low: range.low,
        evidence_high: range.high,
    }
}

fn touches_target(range: &OhlcExecutionRange) -> bool {
    let target = range.tp2_edge.unwrap_or(range.target);
    match range.direction {
        Direction::Buy => range.high >= target,
        Direction::Sell => range.low <= target,
        Direction::None => false,
    }
}

fn touches_partial_target(range: &OhlcExecutionRange) -> bool {
    let Some(tp1) = range.tp1 else {
        return false;
    };
    match range.direction {
        Direction::Buy => range.high >= tp1,
        Direction::Sell => range.low <= tp1,
        Direction::None => false,
    }
}

fn crosses_prospective_breakeven(range: &OhlcExecutionRange) -> bool {
    match range.direction {
        Direction::Buy => range.low <= range.entry_price,
        Direction::Sell => range.high >= range.entry_price,
        Direction::None => false,
    }
}

fn touches_stop(range: &OhlcExecutionRange) -> bool {
    match range.direction {
        Direction::Buy => range.low <= range.stop,
        Direction::Sell => range.high >= range.stop,
        Direction::None => false,
    }
}

fn touches_trailing_stop(range: &OhlcExecutionRange) -> bool {
    let Some(stop) = range.trailing_stop else {
        return false;
    };
    match range.direction {
        Direction::Buy => range.low <= stop,
        Direction::Sell => range.high >= stop,
        Direction::None => false,
    }
}

fn touches_entry(range: &OhlcExecutionRange) -> bool {
    let (Some(low), Some(high)) = (range.entry_low, range.entry_high) else {
        return false;
    };
    range.low <= high && range.high >= low
}
