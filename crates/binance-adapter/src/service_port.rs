use crate::{BinanceError, BinanceRestClient, BinanceWebSocket};
use async_trait::async_trait;
use futures_util::StreamExt;
use market_domain::{AggregateTrade, Candle, Market, MarketSession, Symbol, Timeframe};
use market_service::{MarketSource, MarketStreamSource, ServiceError};

fn source_error(operation: &'static str, error: BinanceError) -> ServiceError {
    ServiceError::Source {
        operation,
        message: error.to_string(),
    }
}

#[async_trait]
impl MarketSource for BinanceRestClient {
    async fn server_time(&self) -> Result<i64, ServiceError> {
        BinanceRestClient::server_time(self)
            .await
            .map_err(|error| source_error("server time", error))
    }

    async fn market(&self, symbol: &Symbol) -> Result<Market, ServiceError> {
        BinanceRestClient::market(self, symbol)
            .await
            .map_err(|error| source_error("market metadata", error))
    }

    async fn klines(
        &self,
        symbol: &Symbol,
        timeframe: Timeframe,
        start_time: Option<i64>,
        end_time: Option<i64>,
        limit: u16,
    ) -> Result<Vec<Candle>, ServiceError> {
        BinanceRestClient::klines(self, symbol, timeframe, start_time, end_time, limit)
            .await
            .map_err(|error| source_error("klines", error))
    }

    async fn aggregate_trades_from(
        &self,
        symbol: &Symbol,
        from_id: i64,
        limit: u16,
    ) -> Result<Vec<AggregateTrade>, ServiceError> {
        BinanceRestClient::aggregate_trades_from(self, symbol, from_id, limit)
            .await
            .map_err(|error| source_error("aggregate trades", error))
    }

    async fn trading_schedule(
        &self,
        session_market: &str,
    ) -> Result<Vec<MarketSession>, ServiceError> {
        BinanceRestClient::trading_schedule(self, session_market)
            .await
            .map_err(|error| source_error("trading schedule", error))
    }
}

#[async_trait]
impl MarketStreamSource for BinanceWebSocket {
    async fn connect(
        &self,
        symbol: &Symbol,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<market_domain::MarketEvent, ServiceError>>,
        ServiceError,
    > {
        let stream = BinanceWebSocket::connect(self, symbol)
            .await
            .map_err(|error| source_error("websocket connect", error))?;
        Ok(Box::pin(stream.map(|result| {
            result.map_err(|error| source_error("websocket stream", error))
        })))
    }
}
