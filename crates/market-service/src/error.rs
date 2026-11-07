#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("storage operation {operation} failed: {message}")]
    Storage {
        operation: &'static str,
        message: String,
    },
    #[error("market source operation {operation} failed: {message}")]
    Source {
        operation: &'static str,
        message: String,
    },
    #[error("invalid market data: {0}")]
    InvalidData(String),
    #[error("service task cancelled")]
    Cancelled,
}
