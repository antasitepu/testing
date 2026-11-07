use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerTimePayload {
    pub server_time: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeInfoPayload {
    pub symbols: Vec<ExchangeSymbolPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExchangeSymbolPayload {
    pub symbol: String,
    pub contract_type: String,
    pub onboard_date: i64,
    pub status: String,
    pub underlying_type: String,
    pub filters: Vec<ExchangeFilterPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExchangeFilterPayload {
    pub filter_type: String,
    pub tick_size: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AggregateTradePayload {
    #[serde(rename = "a")]
    pub aggregate_trade_id: i64,
    #[serde(rename = "p")]
    pub price: String,
    #[serde(rename = "f")]
    pub first_trade_id: i64,
    #[serde(rename = "l")]
    pub last_trade_id: i64,
    #[serde(rename = "T")]
    pub trade_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TradingSchedulePayload {
    pub update_time: i64,
    pub market_schedules: HashMap<String, MarketSchedulePayload>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MarketSchedulePayload {
    pub sessions: Vec<SessionPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionPayload {
    pub start_time: i64,
    pub end_time: i64,
    #[serde(rename = "type")]
    pub session_type: String,
}
