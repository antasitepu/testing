use crate::BinanceError;
use futures_util::{SinkExt, StreamExt, stream::BoxStream};
use market_domain::{
    AggregateTrade, Candle, MarketEvent, MarketSession, SessionStatus, Symbol, Timeframe,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::{str::FromStr, time::Duration};
use tokio::{
    sync::mpsc,
    time::{Instant, MissedTickBehavior},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

const WEBSOCKET_ENDPOINT: &str = "websocket market stream";
pub(crate) const ROTATION_INTERVAL: Duration = Duration::from_secs(23 * 60 * 60 + 55 * 60);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct BinanceWebSocket {
    base_url: Url,
    ping_interval: Duration,
    idle_timeout: Duration,
}

impl BinanceWebSocket {
    pub fn new(base_url: Url) -> Self {
        Self {
            base_url,
            ping_interval: PING_INTERVAL,
            idle_timeout: IDLE_TIMEOUT,
        }
    }

    pub fn with_liveness(mut self, ping_interval: Duration, idle_timeout: Duration) -> Self {
        self.ping_interval = ping_interval;
        self.idle_timeout = idle_timeout;
        self
    }

    pub async fn connect(
        &self,
        symbol: &Symbol,
    ) -> Result<BoxStream<'static, Result<MarketEvent, BinanceError>>, BinanceError> {
        let url = combined_market_stream_url(&self.base_url, symbol)?;
        let idle_timeout = self.idle_timeout;
        let silence_message = format!("no response from Binance for {idle_timeout:?}");
        let (socket, _) = tokio::time::timeout(idle_timeout, connect_async(url.as_str()))
            .await
            .map_err(|_| BinanceError::WebSocket {
                message: silence_message.clone(),
            })?
            .map_err(|error| BinanceError::WebSocket {
                message: error.to_string(),
            })?;
        let ping_interval = self.ping_interval;
        let (sender, receiver) = mpsc::channel(2_048);
        tokio::spawn(async move {
            let mut socket = socket;
            let rotation = tokio::time::sleep(ROTATION_INTERVAL);
            tokio::pin!(rotation);
            // A network path that dies silently never fails a read. Pings make a healthy
            // peer answer even while the market is closed, so silence means a dead path.
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            let mut ping = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
            ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = &mut rotation => {
                        tracing::info!("rotating Binance websocket before 24-hour limit");
                        break;
                    }
                    () = &mut idle => {
                        let _ = sender.send(Err(BinanceError::WebSocket {
                            message: silence_message,
                        })).await;
                        break;
                    }
                    _ = ping.tick() => {
                        if let Err(error) = socket.send(Message::Ping(Default::default())).await {
                            let _ = sender.send(Err(BinanceError::WebSocket {
                                message: error.to_string(),
                            })).await;
                            break;
                        }
                    }
                    message = socket.next() => {
                        let Some(message) = message else {
                            let _ = sender.send(Err(BinanceError::WebSocket {
                                message: "websocket stream ended".to_owned(),
                            })).await;
                            break;
                        };
                        match message {
                            Ok(Message::Text(text)) => match parse_combined_message(text.as_str()) {
                                Ok(Some(event)) => {
                                    if sender.send(Ok(event)).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    if sender.send(Err(error)).await.is_err() {
                                        break;
                                    }
                                }
                            },
                            Ok(Message::Ping(payload)) => {
                                if let Err(error) = socket.send(Message::Pong(payload)).await {
                                    let _ = sender.send(Err(BinanceError::WebSocket {
                                        message: error.to_string(),
                                    })).await;
                                    break;
                                }
                            }
                            Ok(Message::Close(_)) => {
                                let _ = sender.send(Err(BinanceError::WebSocket {
                                    message: "websocket was closed by peer".to_owned(),
                                })).await;
                                break;
                            }
                            Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_)) => {}
                            Err(error) => {
                                let _ = sender.send(Err(BinanceError::WebSocket {
                                    message: error.to_string(),
                                })).await;
                                break;
                            }
                        }
                        // Restarted after the hand-off, so a slow consumer is not
                        // mistaken for a silent peer.
                        idle.as_mut().reset(Instant::now() + idle_timeout);
                    }
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

pub fn combined_market_stream_url(base: &Url, symbol: &Symbol) -> Result<Url, BinanceError> {
    let lower = symbol.as_str().to_ascii_lowercase();
    let streams = [
        format!("{lower}@kline_1m"),
        format!("{lower}@kline_5m"),
        format!("{lower}@kline_15m"),
        format!("{lower}@kline_30m"),
        format!("{lower}@kline_1h"),
        format!("{lower}@kline_4h"),
        format!("{lower}@kline_1d"),
        format!("{lower}@aggTrade"),
        "tradingSession".to_owned(),
    ];
    let mut normalized = base.clone();
    let path = format!("{}/", normalized.path().trim_end_matches('/'));
    normalized.set_path(&path);
    normalized
        .join(&format!("stream?streams={}", streams.join("/")))
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))
}

pub fn parse_combined_message(message: &str) -> Result<Option<MarketEvent>, BinanceError> {
    let envelope: CombinedEnvelope = serde_json::from_str(message)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let event_type = envelope
        .data
        .get("e")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "kline" => parse_kline_event(envelope.data).map(Some),
        "aggTrade" => parse_trade_event(envelope.data).map(Some),
        "CommodityUpdate" => parse_session_event(envelope.data).map(Some),
        _ if envelope.stream == "tradingSession" => Ok(None),
        _ => {
            tracing::debug!(
                stream = envelope.stream,
                event_type,
                "ignoring unknown Binance market event"
            );
            Ok(None)
        }
    }
}

#[derive(Deserialize)]
struct CombinedEnvelope {
    stream: String,
    data: Value,
}

#[derive(Deserialize)]
struct KlineEvent {
    #[serde(rename = "k")]
    kline: KlinePayload,
}

#[derive(Deserialize)]
struct KlinePayload {
    #[serde(rename = "t")]
    open_time: i64,
    #[serde(rename = "T")]
    close_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "i")]
    interval: String,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: Option<String>,
    #[serde(rename = "V")]
    taker_buy_volume: Option<String>,
    #[serde(rename = "x")]
    closed: bool,
}

#[derive(Deserialize)]
struct AggregateTradeEvent {
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "a")]
    aggregate_trade_id: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "f")]
    first_trade_id: i64,
    #[serde(rename = "l")]
    last_trade_id: i64,
    #[serde(rename = "T")]
    trade_time: i64,
}

#[derive(Deserialize)]
struct TradingSessionEvent {
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "t")]
    start_time: i64,
    #[serde(rename = "T")]
    end_time: i64,
    #[serde(rename = "S")]
    status: String,
}

fn parse_kline_event(data: Value) -> Result<MarketEvent, BinanceError> {
    let payload: KlineEvent = serde_json::from_value(data)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let kline = payload.kline;
    let symbol = Symbol::new(kline.symbol)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let timeframe = kline
        .interval
        .parse::<Timeframe>()
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let mut candle = Candle::new(
        symbol,
        timeframe,
        kline.open_time,
        kline.close_time,
        parse_decimal("open", &kline.open)?,
        parse_decimal("high", &kline.high)?,
        parse_decimal("low", &kline.low)?,
        parse_decimal("close", &kline.close)?,
    )
    .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    if let (Some(volume), Some(taker_buy_volume)) = (&kline.volume, &kline.taker_buy_volume) {
        candle = candle
            .with_volume(
                parse_decimal("volume", volume)?,
                parse_decimal("takerBuyVolume", taker_buy_volume)?,
            )
            .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    }
    Ok(if kline.closed {
        MarketEvent::ClosedCandle(candle)
    } else {
        MarketEvent::FormingCandle(candle)
    })
}

fn parse_trade_event(data: Value) -> Result<MarketEvent, BinanceError> {
    let payload: AggregateTradeEvent = serde_json::from_value(data)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let trade = AggregateTrade::new(
        Symbol::new(payload.symbol)
            .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?,
        payload.aggregate_trade_id,
        payload.first_trade_id,
        payload.last_trade_id,
        payload.event_time,
        payload.trade_time,
        parse_decimal("price", &payload.price)?,
    )
    .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    Ok(MarketEvent::AggregateTrade(trade))
}

fn parse_session_event(data: Value) -> Result<MarketEvent, BinanceError> {
    let payload: TradingSessionEvent = serde_json::from_value(data)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    let status = match payload.status.as_str() {
        "REGULAR" => SessionStatus::Regular,
        "NO_TRADING" => SessionStatus::NoTrading,
        other => {
            return Err(BinanceError::invalid(
                WEBSOCKET_ENDPOINT,
                format!("unsupported session status {other}"),
            ));
        }
    };
    let session = MarketSession::new(
        "COMMODITY",
        status,
        payload.start_time,
        payload.end_time,
        payload.event_time,
    )
    .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, error.to_string()))?;
    Ok(MarketEvent::SessionChanged(session))
}

fn parse_decimal(field: &str, raw: &str) -> Result<Decimal, BinanceError> {
    Decimal::from_str(raw)
        .map_err(|error| BinanceError::invalid(WEBSOCKET_ENDPOINT, format!("{field}: {error}")))
}
