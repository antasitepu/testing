use analysis_domain::{
    Direction, DirectiveKind, ExecutionInstruction, ManagementDirective, PlanKind, SignalDefinition,
};
use analysis_plans::validate_mean_reversion_entry;
use execution_domain::{
    ExecutionKind as RangeExecutionKind, OhlcExecutionRange, PositionStage, classify_ohlc_ambiguity,
};
use market_domain::Candle;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The side represented by the source OHLC. A constant spread models quotes;
/// it does not turn exchange trade candles into observed broker quotes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CandlePriceReference {
    #[default]
    Mid,
    Bid,
    Ask,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpreadConfig {
    pub price: Decimal,
    pub candle_reference: CandlePriceReference,
}

impl SpreadConfig {
    pub fn validate(self) -> Result<Self, SpreadConfigError> {
        if self.price < Decimal::ZERO {
            return Err(SpreadConfigError);
        }
        Ok(self)
    }

    fn offset(self, ask: bool) -> Decimal {
        match (self.candle_reference, ask) {
            (CandlePriceReference::Mid, true) => self.price / Decimal::TWO,
            (CandlePriceReference::Mid, false) => -self.price / Decimal::TWO,
            (CandlePriceReference::Bid, true) => self.price,
            (CandlePriceReference::Ask, false) => -self.price,
            _ => Decimal::ZERO,
        }
    }

    pub(crate) fn exit_candle(self, candle: &Candle, direction: Direction) -> Candle {
        self.quote_candle(candle, direction == Direction::Sell)
    }

    fn quote_candle(self, candle: &Candle, ask: bool) -> Candle {
        let offset = self.offset(ask);
        let mut quote = candle.clone();
        quote.open += offset;
        quote.high += offset;
        quote.low += offset;
        quote.close += offset;
        quote
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("spread price must be non-negative")]
pub struct SpreadConfigError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionModel {
    LiveTrade,
    OhlcConservative,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionKind {
    Entry,
    ExitAtNextOpen,
    SuggestClose,
    EmergencyExit,
    ReverseExit,
    ReverseEntry,
    StopLoss,
    PartialTakeProfit,
    TakeProfit,
    EntryCancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulatedExecution {
    pub signal_id: Uuid,
    pub time: i64,
    pub price: Decimal,
    pub kind: ExecutionKind,
    pub ambiguous_intrabar: bool,
}

pub trait ExecutionAdapter {
    fn model(&self) -> ExecutionModel;
    fn on_signal(&mut self, signal: &SignalDefinition, next: &Candle) -> Vec<SimulatedExecution>;
    fn on_candle(
        &mut self,
        candle: &Candle,
        directives: &[ManagementDirective],
    ) -> Vec<SimulatedExecution>;
}

#[derive(Clone)]
struct Position {
    signal_id: Uuid,
    direction: Direction,
    entry: Decimal,
    initial_stop: Decimal,
    stop: Decimal,
    target: Decimal,
    tp1: Option<Decimal>,
    stage: PositionStage,
}

#[derive(Default)]
pub struct OhlcConservativeExecution {
    position: Option<Position>,
    spread: SpreadConfig,
}

impl OhlcConservativeExecution {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_spread(spread: SpreadConfig) -> Result<Self, SpreadConfigError> {
        Ok(Self {
            position: None,
            spread: spread.validate()?,
        })
    }

    fn execution(
        signal_id: Uuid,
        candle: &Candle,
        price: Decimal,
        kind: ExecutionKind,
        ambiguous_intrabar: bool,
    ) -> SimulatedExecution {
        SimulatedExecution {
            signal_id,
            time: candle.open_time,
            price,
            kind,
            ambiguous_intrabar,
        }
    }
}

impl ExecutionAdapter for OhlcConservativeExecution {
    fn model(&self) -> ExecutionModel {
        ExecutionModel::OhlcConservative
    }

    fn on_signal(&mut self, signal: &SignalDefinition, next: &Candle) -> Vec<SimulatedExecution> {
        if self.position.is_some() || next.open_time < signal.effective_at {
            return Vec::new();
        }
        let quote = self
            .spread
            .quote_candle(next, signal.direction == Direction::Buy);
        let next = &quote;
        let entry = match signal.execution_instruction {
            ExecutionInstruction::NextOpen => {
                if signal.plan == PlanKind::MeanReversion
                    && validate_mean_reversion_entry(signal, next.open).is_err()
                {
                    return vec![Self::execution(
                        signal.signal_id,
                        next,
                        next.open,
                        ExecutionKind::EntryCancelled,
                        false,
                    )];
                }
                Some(next.open)
            }
            ExecutionInstruction::LimitZone => signal.entry_zone.as_ref().and_then(|zone| {
                if next.open >= zone.low && next.open <= zone.high {
                    Some(next.open)
                } else if next.open < zone.low && next.high >= zone.low {
                    Some(zone.low)
                } else if next.open > zone.high && next.low <= zone.high {
                    Some(zone.high)
                } else {
                    None
                }
            }),
        };
        let Some(entry) = entry else {
            return Vec::new();
        };
        // Preserve the legacy zero-spread contract; reject invalid price geometry
        // in the opt-in quote model instead of opening a zero/wrong-side risk trade.
        if self.spread.price > Decimal::ZERO
            && match signal.direction {
                Direction::Buy => entry <= signal.stop_loss || entry >= signal.primary_target,
                Direction::Sell => entry >= signal.stop_loss || entry <= signal.primary_target,
                Direction::None => true,
            }
        {
            return vec![Self::execution(
                signal.signal_id,
                next,
                entry,
                ExecutionKind::EntryCancelled,
                false,
            )];
        }
        let initial_risk = (entry - signal.stop_loss).abs();
        let (tp1, target, stage) = signal.execution_plan.as_ref().map_or(
            (None, signal.primary_target, PositionStage::Legacy),
            |plan| {
                let tp1 = match signal.direction {
                    Direction::Buy => entry + initial_risk * plan.tp1.trigger_r,
                    Direction::Sell => entry - initial_risk * plan.tp1.trigger_r,
                    Direction::None => entry,
                };
                let target = match signal.direction {
                    Direction::Buy => plan.tp2_zone.low,
                    Direction::Sell => plan.tp2_zone.high,
                    Direction::None => signal.primary_target,
                };
                (Some(tp1), target, PositionStage::FullPosition)
            },
        );
        self.position = Some(Position {
            signal_id: signal.signal_id,
            direction: signal.direction,
            entry,
            initial_stop: signal.stop_loss,
            stop: signal.stop_loss,
            target,
            tp1,
            stage,
        });
        vec![Self::execution(
            signal.signal_id,
            next,
            entry,
            ExecutionKind::Entry,
            false,
        )]
    }

    fn on_candle(
        &mut self,
        candle: &Candle,
        directives: &[ManagementDirective],
    ) -> Vec<SimulatedExecution> {
        let Some(mut position) = self.position.clone() else {
            return Vec::new();
        };
        let quote = self.spread.exit_candle(candle, position.direction);
        let candle = &quote;
        for directive in directives
            .iter()
            .filter(|directive| directive.effective_at <= candle.open_time)
        {
            match directive.kind {
                DirectiveKind::MoveStop => {
                    if let Some(price) = directive.price {
                        position.stop = price;
                        self.position = Some(position.clone());
                    }
                }
                DirectiveKind::MoveTarget => {
                    if let Some(price) = directive.price {
                        position.target = price;
                        self.position = Some(position.clone());
                    }
                }
                DirectiveKind::SuggestClose => {
                    self.position = None;
                    return vec![Self::execution(
                        position.signal_id,
                        candle,
                        candle.open,
                        ExecutionKind::SuggestClose,
                        false,
                    )];
                }
                DirectiveKind::EmergencyExit => {
                    self.position = None;
                    return vec![Self::execution(
                        position.signal_id,
                        candle,
                        candle.open,
                        ExecutionKind::EmergencyExit,
                        false,
                    )];
                }
                DirectiveKind::ExitAtNextOpen => {
                    self.position = None;
                    return vec![Self::execution(
                        position.signal_id,
                        candle,
                        candle.open,
                        ExecutionKind::ExitAtNextOpen,
                        false,
                    )];
                }
                DirectiveKind::ReverseAtNextOpen => {
                    self.position = None;
                    return vec![Self::execution(
                        position.signal_id,
                        candle,
                        candle.open,
                        ExecutionKind::ReverseExit,
                        false,
                    )];
                }
                DirectiveKind::CancelEntry | DirectiveKind::ExpireEntry => {}
            }
        }
        let decision = classify_ohlc_ambiguity(OhlcExecutionRange {
            direction: position.direction,
            entry_price: position.entry,
            target: position.target,
            tp1: position.tp1,
            tp2_edge: Some(position.target),
            stop: position.initial_stop,
            trailing_stop: (position.stage != PositionStage::FullPosition
                && position.stop != position.initial_stop)
                .then_some(position.stop),
            position_stage: position.stage,
            entry_low: None,
            entry_high: None,
            low: candle.low,
            high: candle.high,
        });
        let stop_at_open = position.stop;
        let mut executions = Vec::new();
        for action in decision.actions {
            match action {
                RangeExecutionKind::PartialTarget => {
                    let Some(tp1) = position.tp1 else {
                        continue;
                    };
                    executions.push(Self::execution(
                        position.signal_id,
                        candle,
                        tp1,
                        ExecutionKind::PartialTakeProfit,
                        decision.ambiguity == execution_domain::Ambiguity::Ambiguous,
                    ));
                    position.stage = PositionStage::Runner;
                    position.stop = position.entry;
                    self.position = Some(position.clone());
                }
                RangeExecutionKind::Stop | RangeExecutionKind::TrailingStop => {
                    self.position = None;
                    executions.push(Self::execution(
                        position.signal_id,
                        candle,
                        if self.spread.price > Decimal::ZERO && position.stop == stop_at_open {
                            match position.direction {
                                Direction::Buy => position.stop.min(candle.open),
                                Direction::Sell => position.stop.max(candle.open),
                                Direction::None => position.stop,
                            }
                        } else {
                            position.stop
                        },
                        ExecutionKind::StopLoss,
                        decision.ambiguity == execution_domain::Ambiguity::Ambiguous,
                    ));
                }
                RangeExecutionKind::Target => {
                    self.position = None;
                    executions.push(Self::execution(
                        position.signal_id,
                        candle,
                        position.target,
                        ExecutionKind::TakeProfit,
                        decision.ambiguity == execution_domain::Ambiguity::Ambiguous,
                    ));
                }
                RangeExecutionKind::Entry | RangeExecutionKind::NoExecution => {}
            }
        }
        executions
    }
}
