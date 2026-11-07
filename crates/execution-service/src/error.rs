#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExecutionError {
    #[error("invalid execution data: {0}")]
    InvalidData(String),
    #[error("execution conflict: {0}")]
    Conflict(String),
    #[error("execution storage error during {operation}: {message}")]
    Storage {
        operation: &'static str,
        message: String,
    },
}
