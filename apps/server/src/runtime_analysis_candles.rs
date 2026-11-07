use market_domain::{Candle, MarketSession};
use market_service::{CandleSessionClass, ServiceError, TradableCandlePolicy};

pub fn filter_runtime_candles(
    candles: Vec<Candle>,
    sessions: &[MarketSession],
    onboard_time: i64,
) -> Result<Vec<Candle>, ServiceError> {
    let policy = TradableCandlePolicy;
    let mut tradable = Vec::with_capacity(candles.len());
    for candle in candles {
        match policy.classify(&candle, sessions) {
            CandleSessionClass::Tradable => tradable.push(candle),
            CandleSessionClass::NoTrading => {}
            // Session coverage starts at listing, so the first higher-timeframe
            // candle can open before it. Later unknown coverage stays an error.
            CandleSessionClass::Unknown if candle.open_time < onboard_time => {}
            CandleSessionClass::Unknown => {
                return Err(ServiceError::InvalidData(format!(
                    "unknown runtime session coverage for {} {} at {}",
                    candle.symbol, candle.timeframe, candle.open_time
                )));
            }
        }
    }
    Ok(tradable)
}
