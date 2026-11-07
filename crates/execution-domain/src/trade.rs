use analysis_domain::{Direction, PlanKind, VersionIdentity};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionModel {
    LiveTrade,
    OhlcConservative,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReconciliationSource {
    ExactTrade,
    OhlcReconciled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ambiguity {
    None,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeState {
    NotEntered,
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionStage {
    #[default]
    Legacy,
    FullPosition,
    Runner,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeHealth {
    #[default]
    Healthy,
    Weakening,
    Invalid,
}

impl TradeState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Closed)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrackedLevelKind {
    EntryZone,
    ProjectedEntry,
    ActualEntry,
    PrimaryTarget,
    InitialStop,
    TrailingStop,
    Reference,
}

impl TrackedLevelKind {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::PrimaryTarget | Self::InitialStop | Self::TrailingStop
        )
    }

    pub const fn is_executable(self) -> bool {
        !matches!(self, Self::Reference)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketCursor {
    pub symbol: Symbol,
    pub last_processed_agg_trade_id: i64,
    pub last_trade_time: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTrade {
    pub virtual_trade_id: Uuid,
    pub signal_id: Uuid,
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub timeframe: Timeframe,
    pub direction: Direction,
    pub trade_state: TradeState,
    #[serde(default)]
    pub position_stage: PositionStage,
    #[serde(default)]
    pub trade_health: TradeHealth,
    pub actual_entry_time: i64,
    pub actual_entry_price: Decimal,
    pub exit_time: Option<i64>,
    pub exit_price: Option<Decimal>,
    pub primary_target: Decimal,
    pub reference_levels: Vec<Decimal>,
    pub initial_stop: Decimal,
    pub active_stop: Decimal,
    #[serde(default)]
    pub thesis_invalidation: Option<Decimal>,
    #[serde(default)]
    pub tp1_price: Option<Decimal>,
    #[serde(default)]
    pub tp1_r: Option<Decimal>,
    #[serde(default)]
    pub tp1_fraction: Option<Decimal>,
    #[serde(default)]
    pub tp1_filled_at: Option<i64>,
    #[serde(default = "decimal_one")]
    pub remaining_fraction: Decimal,
    #[serde(default)]
    pub realized_fraction: Decimal,
    #[serde(default)]
    pub realized_r: Decimal,
    #[serde(default)]
    pub tp2_zone_low: Option<Decimal>,
    #[serde(default)]
    pub tp2_zone_high: Option<Decimal>,
    #[serde(default)]
    pub last_management_effective_at: Option<i64>,
    #[serde(default)]
    pub last_management_candle_time: Option<i64>,
    pub versions: VersionIdentity,
    pub execution_model: ExecutionModel,
    pub reconciliation_source: Option<ReconciliationSource>,
    pub ambiguity: Ambiguity,
    pub gross_r: Option<Decimal>,
    pub mfe_r: Decimal,
    pub mae_r: Decimal,
    pub duration_ms: Option<i64>,
    pub exit_reason: Option<String>,
}

fn decimal_one() -> Decimal {
    Decimal::ONE
}
