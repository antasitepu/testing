#![forbid(unsafe_code)]

mod error;
mod rest;
mod service_port;
mod websocket;
mod wire;

pub use error::BinanceError;
pub use rest::BinanceRestClient;
pub use websocket::{BinanceWebSocket, combined_market_stream_url, parse_combined_message};
