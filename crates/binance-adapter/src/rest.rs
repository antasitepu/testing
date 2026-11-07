use crate::{
    BinanceError,
    wire::{AggregateTradePayload, ExchangeInfoPayload, ServerTimePayload, TradingSchedulePayload},
};
use market_domain::{
    AggregateTrade, Candle, Market, MarketSession, MarketStatus, SessionStatus, Symbol, Timeframe,
};
use reqwest::{Client, Response};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{str::FromStr, time::Duration};
use url::Url;

const SERVER_TIME_ENDPOINT: &str = "/fapi/v1/time";
const EXCHANGE_INFO_ENDPOINT: &str = "/fapi/v1/exchangeInfo";
const KLINES_ENDPOINT: &str = "/fapi/v1/klines";
const AGGREGATE_TRADES_ENDPOINT: &str = "/fapi/v1/aggTrades";
const TRADING_SCHEDULE_ENDPOINT: &str = "/fapi/v1/tradingSchedule";
const ERROR_BODY_LIMIT: usize = 2_048;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_INFO_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BinanceRestClient {
    client: Client,
    base_url: Url,
}

impl BinanceRestClient {
    pub fn new(client: Client, base_url: Url) -> Self {
        Self { client, base_url }
    }

    pub async fn server_time(&self) -> Result<i64, BinanceError> {
        let payload: ServerTimePayload = self.get_json(SERVER_TIME_ENDPOINT, &[]).await?;
        if payload.server_time < 0 {
            return Err(BinanceError::invalid(
                SERVER_TIME_ENDPOINT,
                "serverTime must not be negative",
            ));
        }
        Ok(payload.server_time)
    }

    pub async fn market(&self, symbol: &Symbol) -> Result<Market, BinanceError> {
        let payload: ExchangeInfoPayload = self.get_json(EXCHANGE_INFO_ENDPOINT, &[]).await?;
        let item = payload
            .symbols
            .into_iter()
            .find(|candidate| candidate.symbol == symbol.as_str())
            .ok_or_else(|| {
                BinanceError::invalid(
                    EXCHANGE_INFO_ENDPOINT,
                    format!("symbol {symbol} is missing"),
                )
            })?;
        if item.contract_type != "TRADIFI_PERPETUAL" {
            return Err(BinanceError::invalid(
                EXCHANGE_INFO_ENDPOINT,
                format!(
                    "symbol {symbol} has unsupported contract type {}",
                    item.contract_type
                ),
            ));
        }
        let tick_size = item
            .filters
            .iter()
            .find(|filter| filter.filter_type == "PRICE_FILTER")
            .and_then(|filter| filter.tick_size.as_deref())
            .ok_or_else(|| {
                BinanceError::invalid(
                    EXCHANGE_INFO_ENDPOINT,
                    format!("symbol {symbol} has no PRICE_FILTER tickSize"),
                )
            })
            .and_then(|raw| parse_decimal(EXCHANGE_INFO_ENDPOINT, "tickSize", raw))?;
        let status = match item.status.as_str() {
            "TRADING" => MarketStatus::Trading,
            "HALTED" => MarketStatus::Halted,
            _ => MarketStatus::Unknown,
        };
        Market::new(
            symbol.clone(),
            true,
            status,
            tick_size,
            item.onboard_date,
            item.underlying_type,
        )
        .map_err(|error| BinanceError::invalid(EXCHANGE_INFO_ENDPOINT, error.to_string()))
    }

    pub async fn klines(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start_time: Option<i64>,
        end_time: Option<i64>,
        limit: u16,
    ) -> Result<Vec<Candle>, BinanceError> {
        if !(1..=1_500).contains(&limit) {
            return Err(BinanceError::invalid(
                KLINES_ENDPOINT,
                "limit must be between 1 and 1500",
            ));
        }
        let mut query = vec![
            ("symbol", symbol.as_str().to_owned()),
            ("interval", timeframe.as_str().to_owned()),
            ("limit", limit.to_string()),
        ];
        if let Some(start_time) = start_time {
            query.push(("startTime", start_time.to_string()));
        }
        if let Some(end_time) = end_time {
            query.push(("endTime", end_time.to_string()));
        }
        let rows: Vec<Value> = self.get_json(KLINES_ENDPOINT, &query).await?;
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| parse_kline(symbol, timeframe, index, row))
            .collect()
    }

    pub async fn aggregate_trades_from(
        &self,
        symbol: &Symbol,
        from_id: i64,
        limit: u16,
    ) -> Result<Vec<AggregateTrade>, BinanceError> {
        if from_id < 0 {
            return Err(BinanceError::invalid(
                AGGREGATE_TRADES_ENDPOINT,
                "fromId must not be negative",
            ));
        }
        if !(1..=1_000).contains(&limit) {
            return Err(BinanceError::invalid(
                AGGREGATE_TRADES_ENDPOINT,
                "limit must be between 1 and 1000",
            ));
        }
        let query = vec![
            ("symbol", symbol.as_str().to_owned()),
            ("fromId", from_id.to_string()),
            ("limit", limit.to_string()),
        ];
        let rows: Vec<AggregateTradePayload> =
            self.get_json(AGGREGATE_TRADES_ENDPOINT, &query).await?;
        rows.into_iter()
            .map(|row| {
                let price = parse_decimal(AGGREGATE_TRADES_ENDPOINT, "price", &row.price)?;
                AggregateTrade::new(
                    symbol.clone(),
                    row.aggregate_trade_id,
                    row.first_trade_id,
                    row.last_trade_id,
                    row.trade_time,
                    row.trade_time,
                    price,
                )
                .map_err(|error| {
                    BinanceError::invalid(AGGREGATE_TRADES_ENDPOINT, error.to_string())
                })
            })
            .collect()
    }

    pub async fn trading_schedule(
        &self,
        session_market: &str,
    ) -> Result<Vec<MarketSession>, BinanceError> {
        if session_market.is_empty() {
            return Err(BinanceError::invalid(
                TRADING_SCHEDULE_ENDPOINT,
                "session market must not be empty",
            ));
        }
        let payload: TradingSchedulePayload = self.get_json(TRADING_SCHEDULE_ENDPOINT, &[]).await?;
        let schedule = payload
            .market_schedules
            .get(session_market)
            .ok_or_else(|| {
                BinanceError::invalid(
                    TRADING_SCHEDULE_ENDPOINT,
                    format!("market schedule {session_market} is missing"),
                )
            })?;
        schedule
            .sessions
            .iter()
            .map(|session| {
                let status = match session.session_type.as_str() {
                    "REGULAR" => SessionStatus::Regular,
                    "NO_TRADING" => SessionStatus::NoTrading,
                    other => {
                        return Err(BinanceError::invalid(
                            TRADING_SCHEDULE_ENDPOINT,
                            format!("unsupported session type {other}"),
                        ));
                    }
                };
                MarketSession::new(
                    session_market,
                    status,
                    session.start_time,
                    session.end_time,
                    payload.update_time,
                )
                .map_err(|error| {
                    BinanceError::invalid(TRADING_SCHEDULE_ENDPOINT, error.to_string())
                })
            })
            .collect()
    }

    async fn get_json<T>(
        &self,
        endpoint: &'static str,
        query: &[(&str, String)],
    ) -> Result<T, BinanceError>
    where
        T: DeserializeOwned,
    {
        let url = self
            .base_url
            .join(endpoint.trim_start_matches('/'))
            .map_err(|error| BinanceError::invalid(endpoint, error.to_string()))?;
        let response = self
            .client
            .get(url)
            .query(query)
            .timeout(request_timeout(endpoint))
            .send()
            .await
            .map_err(|source| BinanceError::Request { endpoint, source })?;
        let response = ensure_success(endpoint, response).await?;
        response
            .json::<T>()
            .await
            .map_err(|source| BinanceError::Request { endpoint, source })
    }
}

fn request_timeout(endpoint: &str) -> Duration {
    if endpoint == EXCHANGE_INFO_ENDPOINT {
        EXCHANGE_INFO_REQUEST_TIMEOUT
    } else {
        DEFAULT_REQUEST_TIMEOUT
    }
}

async fn ensure_success(
    endpoint: &'static str,
    response: Response,
) -> Result<Response, BinanceError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("unable to read response body: {error}"));
    Err(BinanceError::Status {
        endpoint,
        status,
        body: body.chars().take(ERROR_BODY_LIMIT).collect(),
    })
}

fn parse_kline(
    symbol: &Symbol,
    timeframe: Timeframe,
    row_index: usize,
    row: Value,
) -> Result<Candle, BinanceError> {
    let values = row.as_array().ok_or_else(|| {
        BinanceError::invalid(KLINES_ENDPOINT, format!("row {row_index} is not an array"))
    })?;
    if values.len() < 7 {
        return Err(BinanceError::invalid(
            KLINES_ENDPOINT,
            format!("row {row_index} has fewer than 7 fields"),
        ));
    }
    let open_time = json_i64(KLINES_ENDPOINT, row_index, "open time", &values[0])?;
    let open = json_decimal(KLINES_ENDPOINT, row_index, "open", &values[1])?;
    let high = json_decimal(KLINES_ENDPOINT, row_index, "high", &values[2])?;
    let low = json_decimal(KLINES_ENDPOINT, row_index, "low", &values[3])?;
    let close = json_decimal(KLINES_ENDPOINT, row_index, "close", &values[4])?;
    let close_time = json_i64(KLINES_ENDPOINT, row_index, "close time", &values[6])?;
    Candle::new(
        symbol.clone(),
        timeframe,
        open_time,
        close_time,
        open,
        high,
        low,
        close,
    )
    .map_err(|error| BinanceError::invalid(KLINES_ENDPOINT, format!("row {row_index}: {error}")))
}

fn json_i64(
    endpoint: &'static str,
    row_index: usize,
    field: &str,
    value: &Value,
) -> Result<i64, BinanceError> {
    value.as_i64().ok_or_else(|| {
        BinanceError::invalid(
            endpoint,
            format!("row {row_index} {field} is not an integer"),
        )
    })
}

fn json_decimal(
    endpoint: &'static str,
    row_index: usize,
    field: &str,
    value: &Value,
) -> Result<Decimal, BinanceError> {
    let raw = value.as_str().ok_or_else(|| {
        BinanceError::invalid(
            endpoint,
            format!("row {row_index} {field} is not a decimal string"),
        )
    })?;
    parse_decimal(endpoint, field, raw)
}

fn parse_decimal(endpoint: &'static str, field: &str, raw: &str) -> Result<Decimal, BinanceError> {
    Decimal::from_str(raw)
        .map_err(|error| BinanceError::invalid(endpoint, format!("{field}: {error}")))
}

#[cfg(test)]
mod request_timeout_tests {
    use super::{EXCHANGE_INFO_ENDPOINT, KLINES_ENDPOINT, request_timeout};
    use std::time::Duration;

    #[test]
    fn exchange_metadata_allows_a_larger_body_timeout_without_relaxing_market_requests() {
        assert_eq!(
            request_timeout(EXCHANGE_INFO_ENDPOINT),
            Duration::from_secs(30)
        );
        assert_eq!(request_timeout(KLINES_ENDPOINT), Duration::from_secs(10));
    }
}
