#![forbid(unsafe_code)]

mod barrier;
mod cache;
mod engine;
mod indicators;
mod liquidity;
mod range;
pub mod range_forecast;
mod session_range;
mod structure;

pub use barrier::{
    BarrierError, BarrierResult, ClosedTimeframeFacts, EvaluationBarrier, MissingDependency,
};
pub use cache::{SnapshotCache, SnapshotCacheKey};
pub use engine::{FactLedger, FactLedgerError, SnapshotBuilder, SnapshotError, SnapshotRequest};
pub use indicators::{
    BollingerFact, ChandelierFact, DivergenceKind, DonchianFact, EmaOutput, IndicatorError,
    IndicatorOutput, RegularDivergence, RsiPivot, calculate_adx, calculate_atr,
    calculate_bollinger, calculate_chandelier, calculate_ema, calculate_previous_donchian,
    calculate_rsi, detect_regular_divergence, next_long_stop, next_short_stop,
};
pub use liquidity::{
    FairValueGap, FairValueGapDirection, LiquidityError, LiquidityPool, LiquiditySide,
    LiquiditySweep, detect_fair_value_gaps, detect_liquidity_pools, detect_liquidity_sweeps,
    fvg_lifecycle, related_sequence,
};
pub use range::{DealingRange, RangeError, RangeRegion, build_dealing_range};
pub use session_range::{SessionRangeFact, SessionRangeParameters, detect_session_ranges};
pub use structure::{
    ConfirmedSwing, Displacement, DisplacementParameters, StructureAnalysis, StructureError,
    StructureEvent, StructureEventKind, SwingKind, SwingLabel, SwingParameters,
    detect_displacements, detect_displacements_with_atr, detect_structure,
};
