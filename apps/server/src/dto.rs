use market_domain::{
    AggregateTrade, Candle, LiveStatus, Market, MarketStatus, Symbol, SyncRunStatus, SyncStatus,
    Timeframe,
};
use market_service::MarketSnapshot;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketDto {
    pub symbol: Symbol,
    pub enabled: bool,
    pub status: MarketStatus,
    pub price_tick: String,
    pub onboard_time: i64,
    pub session_market: String,
    pub intervals: Vec<Timeframe>,
}

impl From<Market> for MarketDto {
    fn from(market: Market) -> Self {
        Self {
            symbol: market.symbol,
            enabled: market.enabled,
            status: market.status,
            price_tick: market.price_tick.to_string(),
            onboard_time: market.onboard_time,
            session_market: market.session_market,
            intervals: Timeframe::ALL.to_vec(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandleDto {
    pub open_time: i64,
    pub close_time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: Option<String>,
    pub taker_buy_volume: Option<String>,
}

impl From<Candle> for CandleDto {
    fn from(candle: Candle) -> Self {
        Self {
            open_time: candle.open_time,
            close_time: candle.close_time,
            open: candle.open.to_string(),
            high: candle.high.to_string(),
            low: candle.low.to_string(),
            close: candle.close.to_string(),
            volume: candle.volume.map(|value| value.to_string()),
            taker_buy_volume: candle.taker_buy_volume.map(|value| value.to_string()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandlePageDto {
    pub data: Vec<CandleDto>,
    pub next_before: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    pub symbol: Symbol,
    pub interval: Timeframe,
    pub oldest_synced_time: Option<i64>,
    pub latest_synced_time: Option<i64>,
    pub full_history_complete: bool,
    pub recent_sync_status: SyncRunStatus,
    pub backfill_status: SyncRunStatus,
    pub last_error: Option<String>,
}

impl From<SyncStatus> for SyncStatusDto {
    fn from(status: SyncStatus) -> Self {
        Self {
            symbol: status.symbol,
            interval: status.timeframe,
            oldest_synced_time: status.oldest_synced_time,
            latest_synced_time: status.latest_synced_time,
            full_history_complete: status.full_history_complete,
            recent_sync_status: status.recent_sync_status,
            backfill_status: status.backfill_status,
            last_error: status.last_error,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeDto {
    pub aggregate_trade_id: i64,
    pub event_time: i64,
    pub trade_time: i64,
    pub price: String,
}

impl From<AggregateTrade> for TradeDto {
    fn from(trade: AggregateTrade) -> Self {
        Self {
            aggregate_trade_id: trade.aggregate_trade_id,
            event_time: trade.event_time,
            trade_time: trade.trade_time,
            price: trade.price.to_string(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDto {
    pub forming: Option<CandleDto>,
    pub latest_trade: Option<TradeDto>,
    pub status: LiveStatus,
}

impl From<MarketSnapshot> for SnapshotDto {
    fn from(snapshot: MarketSnapshot) -> Self {
        Self {
            forming: snapshot.forming.map(Into::into),
            latest_trade: snapshot.latest_trade.map(Into::into),
            status: snapshot.status,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub status: LiveStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResyncDto {
    pub reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorDto {
    pub code: &'static str,
    pub message: String,
}
