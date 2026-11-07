#[derive(Debug, thiserror::Error)]
pub enum BinanceError {
    #[error("request failed for {endpoint}: {source}")]
    Request {
        endpoint: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("binance returned {status} for {endpoint}: {body}")]
    Status {
        endpoint: &'static str,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("invalid binance payload for {endpoint}: {message}")]
    InvalidPayload {
        endpoint: &'static str,
        message: String,
    },
    #[error("websocket transport failed: {message}")]
    WebSocket { message: String },
}

impl BinanceError {
    pub(crate) fn invalid(endpoint: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidPayload {
            endpoint,
            message: message.into(),
        }
    }
}
