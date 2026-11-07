use market_domain::Timeframe;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::ops::Range;

pub(crate) const LOW_TIMEFRAMES: [Timeframe; 3] = [Timeframe::M1, Timeframe::M5, Timeframe::M15];
pub(crate) const MINIMUM_OOS_TRADES: u64 = 30;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TuningStage {
    Train,
    Validation,
    Test,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChronologicalBoundaries {
    pub train: Range<usize>,
    pub validation: Range<usize>,
    pub test: Range<usize>,
}

impl ChronologicalBoundaries {
    pub(crate) fn new(candle_count: usize) -> Result<Self, &'static str> {
        let train_end = candle_count * 60 / 100;
        let validation_end = candle_count * 80 / 100;
        if train_end == 0 || validation_end <= train_end || validation_end >= candle_count {
            return Err(
                "chronological split requires non-empty train, validation, and test ranges",
            );
        }
        Ok(Self {
            train: 0..train_end,
            validation: train_end..validation_end,
            test: validation_end..candle_count,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StageMetrics {
    pub total_trades: u64,
    pub win_rate: Decimal,
    pub net_r: Decimal,
    pub expectancy_r: Decimal,
    pub profit_factor: Decimal,
    pub max_drawdown_r: Decimal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileEvidence {
    pub baseline_test: StageMetrics,
    pub candidate_validation: StageMetrics,
    pub candidate_test: StageMetrics,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PromotionDecision {
    Changed,
    KeptBaseline,
    InsufficientSample,
}

impl PromotionDecision {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "CHANGED",
            Self::KeptBaseline => "KEPT_BASELINE",
            Self::InsufficientSample => "INSUFFICIENT_SAMPLE",
        }
    }
}

pub(crate) fn evaluate_promotion(evidence: &ProfileEvidence) -> PromotionDecision {
    if evidence.candidate_validation.total_trades < MINIMUM_OOS_TRADES
        || evidence.candidate_test.total_trades < MINIMUM_OOS_TRADES
    {
        return PromotionDecision::InsufficientSample;
    }
    let positive = |metrics: &StageMetrics| {
        metrics.net_r > Decimal::ZERO
            && metrics.expectancy_r > Decimal::ZERO
            && metrics.profit_factor > Decimal::ONE
    };
    if !positive(&evidence.candidate_validation)
        || !positive(&evidence.candidate_test)
        || evidence.candidate_test.net_r <= evidence.baseline_test.net_r
        || evidence.candidate_test.max_drawdown_r > evidence.baseline_test.max_drawdown_r
    {
        return PromotionDecision::KeptBaseline;
    }
    PromotionDecision::Changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn metrics(
        trades: u64,
        net_r: i64,
        expectancy_hundredths: i64,
        pf_tenths: i64,
        dd: i64,
    ) -> StageMetrics {
        StageMetrics {
            total_trades: trades,
            win_rate: Decimal::new(50, 2),
            net_r: Decimal::new(net_r, 0),
            expectancy_r: Decimal::new(expectancy_hundredths, 2),
            profit_factor: Decimal::new(pf_tenths, 1),
            max_drawdown_r: Decimal::new(dd, 0),
        }
    }

    #[test]
    fn scope_is_exactly_the_three_low_timeframes() {
        assert_eq!(
            LOW_TIMEFRAMES,
            [Timeframe::M1, Timeframe::M5, Timeframe::M15]
        );
    }

    #[test]
    fn chronological_boundaries_are_sixty_twenty_twenty() {
        let split = ChronologicalBoundaries::new(101).expect("split");
        assert_eq!(split.train, 0..60);
        assert_eq!(split.validation, 60..80);
        assert_eq!(split.test, 80..101);
    }

    #[test]
    fn promotion_requires_minimum_validation_and_test_samples() {
        let evidence = ProfileEvidence {
            baseline_test: metrics(40, 2, 5, 11, 5),
            candidate_validation: metrics(29, 5, 10, 12, 4),
            candidate_test: metrics(40, 6, 12, 13, 4),
        };
        assert_eq!(
            evaluate_promotion(&evidence),
            PromotionDecision::InsufficientSample
        );
    }

    #[test]
    fn promotion_rejects_negative_out_of_sample_performance() {
        let evidence = ProfileEvidence {
            baseline_test: metrics(40, 2, 5, 11, 5),
            candidate_validation: metrics(40, -1, -1, 9, 4),
            candidate_test: metrics(40, 6, 12, 13, 4),
        };
        assert_eq!(
            evaluate_promotion(&evidence),
            PromotionDecision::KeptBaseline
        );
    }

    #[test]
    fn promotion_requires_test_return_to_beat_baseline() {
        let evidence = ProfileEvidence {
            baseline_test: metrics(40, 8, 5, 11, 5),
            candidate_validation: metrics(40, 5, 10, 12, 4),
            candidate_test: metrics(40, 8, 12, 13, 4),
        };
        assert_eq!(
            evaluate_promotion(&evidence),
            PromotionDecision::KeptBaseline
        );
    }

    #[test]
    fn promotion_rejects_worse_test_drawdown() {
        let evidence = ProfileEvidence {
            baseline_test: metrics(40, 2, 5, 11, 5),
            candidate_validation: metrics(40, 5, 10, 12, 4),
            candidate_test: metrics(40, 8, 12, 13, 6),
        };
        assert_eq!(
            evaluate_promotion(&evidence),
            PromotionDecision::KeptBaseline
        );
    }

    #[test]
    fn promotion_accepts_positive_oos_candidate_that_beats_baseline_with_controlled_drawdown() {
        let evidence = ProfileEvidence {
            baseline_test: metrics(40, 2, 5, 11, 5),
            candidate_validation: metrics(40, 5, 10, 12, 4),
            candidate_test: metrics(40, 8, 12, 13, 5),
        };
        assert_eq!(evaluate_promotion(&evidence), PromotionDecision::Changed);
        assert_eq!(PromotionDecision::Changed.as_str(), "CHANGED");
    }
}
