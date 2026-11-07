#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AnalysisServiceError {
    #[error("storage operation {operation} failed: {message}")]
    Storage {
        operation: &'static str,
        message: String,
    },
    #[error("analysis conflict: {0}")]
    Conflict(String),
    #[error("analysis record not found: {0}")]
    NotFound(String),
    #[error("invalid analysis data: {0}")]
    InvalidData(String),
}
