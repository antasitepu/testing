use analysis_domain::PlanKind;
use analysis_plans::{
    IctEvaluator, MeanReversionEvaluator, PlanEvaluator, QuasimodoEvaluator, RsiEvaluator,
    SmcEvaluator, TrendBreakoutEvaluator,
};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct PlanRegistry {
    evaluators: BTreeMap<PlanKind, Arc<dyn PlanEvaluator>>,
}

impl PlanRegistry {
    pub fn new(evaluators: impl IntoIterator<Item = Arc<dyn PlanEvaluator>>) -> Self {
        Self {
            evaluators: evaluators
                .into_iter()
                .map(|evaluator| (evaluator.kind(), evaluator))
                .collect(),
        }
    }

    pub fn get(&self, plan: PlanKind) -> Option<&Arc<dyn PlanEvaluator>> {
        self.evaluators.get(&plan)
    }
}

impl Default for PlanRegistry {
    fn default() -> Self {
        Self::new([
            Arc::new(IctEvaluator) as Arc<dyn PlanEvaluator>,
            Arc::new(SmcEvaluator),
            Arc::new(RsiEvaluator),
            Arc::new(QuasimodoEvaluator),
            Arc::new(TrendBreakoutEvaluator),
            Arc::new(MeanReversionEvaluator),
        ])
    }
}
