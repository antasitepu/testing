use analysis_domain::{PlanKind, PlanProfileKey};
use market_domain::Timeframe;
use serde::Serialize;

pub use crate::runtime::profiles::{RuntimeProfileCatalog, RuntimeProfileCatalogEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum RuntimeTuningStatus {
    Baseline,
    Locked,
}

impl RuntimeTuningStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "BASELINE",
            Self::Locked => "LOCKED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeProfileTuning {
    pub status: RuntimeTuningStatus,
    pub candidate_label: Option<&'static str>,
    pub source: &'static str,
}

pub(crate) fn tuning_metadata(
    key: &PlanProfileKey,
    parameter_version: &str,
) -> RuntimeProfileTuning {
    let candidate_label = long_run_candidate_label(key.plan, key.target_timeframe);
    let locked = candidate_label.is_some() || !parameter_version.ends_with("-runtime-v1");
    RuntimeProfileTuning {
        status: if locked {
            RuntimeTuningStatus::Locked
        } else {
            RuntimeTuningStatus::Baseline
        },
        candidate_label,
        source: if candidate_label.is_some() {
            "long-run-20260702"
        } else if locked {
            "runtime-profile-lock"
        } else {
            "runtime-default"
        },
    }
}

pub(crate) fn long_run_candidate_label(
    plan: PlanKind,
    timeframe: Timeframe,
) -> Option<&'static str> {
    match (plan, timeframe) {
        (PlanKind::Ict, Timeframe::M1) => Some("ict-fvgnearest-sweepopposite-rr1.5-atr0.2-entry10"),
        (PlanKind::Ict, Timeframe::M5) => Some("ict-fvgnearest-sweepsame-rr0.8-atr0.1-entry20"),
        (PlanKind::Rsi, Timeframe::M1) => Some("rsi-os35-ob65-divfalse-rr1.5-atr0.2"),
        (PlanKind::Rsi, Timeframe::M5) => Some("rsi-os30-ob70-divfalse-rr1.5-atr0.2"),
        (PlanKind::Rsi, Timeframe::H4) => Some("rsi-os35-ob65-divfalse-rr1.25-atr0.35"),
        (PlanKind::TrendBreakout, Timeframe::M1) => Some("trend-htfon-adx20-stop2-rr2-trail0.75"),
        (PlanKind::TrendBreakout, Timeframe::M5) => {
            Some("trend-htfon-adx20-stop1.5-rr1.5-trail0.75")
        }
        (PlanKind::TrendBreakout, Timeframe::M15) => Some("trend-htfoff-adx20-stop2-rr2-trail0.75"),
        (PlanKind::TrendBreakout, Timeframe::M30) => Some("trend-htfon-adx25-stop2-rr2-trail1"),
        (PlanKind::TrendBreakout, Timeframe::H1) => {
            Some("trend-htfoff-adx25-stop2-rr1.5-trail0.75")
        }
        (PlanKind::TrendBreakout, Timeframe::H4) => {
            Some("trend-htfoff-adx20-stop1.5-rr2-trail0.75")
        }
        (PlanKind::MeanReversion, Timeframe::M1) => Some("mean-z1.5-adx20-bw0.05-rr1-atr0.1"),
        (PlanKind::MeanReversion, Timeframe::M5) => Some("mean-z1.5-adx20-bw0.05-rr0.8-atr0.05"),
        (PlanKind::MeanReversion, Timeframe::M15) => Some("mean-z1.5-adx25-bw0.05-rr1-atr0.1"),
        (PlanKind::MeanReversion, Timeframe::H4) => Some("mean-z1.5-adx25-bw0.05-rr1-atr0.1"),
        _ => None,
    }
}
