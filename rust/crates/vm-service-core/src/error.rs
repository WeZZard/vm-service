//! Client-visible service error.
//!
//! Mirrors the Python `OpError` class: a message that is safe to return to an
//! HTTP client or CLI user.

/// An operation failure whose message reaches the caller.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct OpError(pub String);

impl OpError {
    /// Construct an error from any displayable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<std::io::Error> for OpError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Convenience result alias for service operations.
pub type OpResult<T> = Result<T, OpError>;
