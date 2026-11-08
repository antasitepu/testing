use crate::execution_wake::ExecutionWakeSource;
use crate::range_forecast::RangeForecaster;
use crate::runtime::health::RuntimeHealthReader;
use analysis_service::{BacktestStore, ConfigurationStore, RejectedCandidateStore, SignalStore};
use execution_service::{MonitoringStore, ScalpingAuditStore, VirtualTradeStore};
use market_service::{CandleStore, MarketHub, MarketStore, SyncStore};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub markets: Arc<dyn MarketStore>,
    pub candles: Arc<dyn CandleStore>,
    pub sync: Arc<dyn SyncStore>,
    pub hub: Arc<MarketHub>,
    pub analysis: Option<Arc<dyn SignalStore>>,
    pub backtests: Option<Arc<dyn BacktestStore>>,
    pub configurations: Option<Arc<dyn ConfigurationStore>>,
    pub rejected_candidates: Option<Arc<dyn RejectedCandidateStore>>,
    pub monitoring: Option<Arc<dyn MonitoringStore>>,
    pub monitoring_wake: Option<ExecutionWakeSource>,
    pub virtual_trades: Option<Arc<dyn VirtualTradeStore>>,
    pub scalping_audits: Option<Arc<dyn ScalpingAuditStore>>,
    pub runtime_health: RuntimeHealthReader,
    /// Display-only range forecast; `None` when no model file is configured.
    pub range_forecast: Option<Arc<RangeForecaster>>,
    pub default_history_limit: u16,
}

impl AppState {
    pub fn new(
        markets: Arc<dyn MarketStore>,
        candles: Arc<dyn CandleStore>,
        sync: Arc<dyn SyncStore>,
        hub: Arc<MarketHub>,
        default_history_limit: u16,
    ) -> Self {
        Self {
            markets,
            candles,
            sync,
            hub,
            analysis: None,
            backtests: None,
            configurations: None,
            rejected_candidates: None,
            monitoring: None,
            monitoring_wake: None,
            virtual_trades: None,
            scalping_audits: None,
            runtime_health: RuntimeHealthReader::starting(0),
            range_forecast: None,
            default_history_limit,
        }
    }

    pub fn with_analysis(mut self, analysis: Arc<dyn SignalStore>) -> Self {
        self.analysis = Some(analysis);
        self
    }

    pub fn with_backtests(
        mut self,
        backtests: Arc<dyn BacktestStore>,
        configurations: Arc<dyn ConfigurationStore>,
    ) -> Self {
        self.backtests = Some(backtests);
        self.configurations = Some(configurations);
        self
    }

    pub fn with_rejected_candidates(
        mut self,
        rejected_candidates: Arc<dyn RejectedCandidateStore>,
    ) -> Self {
        self.rejected_candidates = Some(rejected_candidates);
        self
    }

    pub fn with_monitoring(mut self, monitoring: Arc<dyn MonitoringStore>) -> Self {
        self.monitoring = Some(monitoring);
        self
    }

    pub fn with_monitoring_wake(mut self, monitoring_wake: ExecutionWakeSource) -> Self {
        self.monitoring_wake = Some(monitoring_wake);
        self
    }

    pub fn with_virtual_trades(mut self, virtual_trades: Arc<dyn VirtualTradeStore>) -> Self {
        self.virtual_trades = Some(virtual_trades);
        self
    }

    pub fn with_scalping_audits(mut self, scalping_audits: Arc<dyn ScalpingAuditStore>) -> Self {
        self.scalping_audits = Some(scalping_audits);
        self
    }

    pub fn with_runtime_health(mut self, runtime_health: RuntimeHealthReader) -> Self {
        self.runtime_health = runtime_health;
        self
    }

    pub fn with_range_forecast(mut self, range_forecast: Arc<RangeForecaster>) -> Self {
        self.range_forecast = Some(range_forecast);
        self
    }
}
