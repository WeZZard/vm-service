//! Client-visible service error.
//!
//! Mirrors the Python `OpError` class: a message that is safe to return to an
//! HTTP client or CLI user.

/// An operation failure whose message reaches the caller.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct OpError {
    /// The message returned to the caller.
    pub message: String,
    /// Whether the operation may have taken effect in the guest although it
    /// failed (`docs/arch/guest-session.md`, GS-5). The daemon reports it as
    /// `"outcome": "uncertain"`.
    pub uncertain: bool,
}

impl OpError {
    /// Construct an error from any displayable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain: false,
        }
    }

    /// Construct an error for an operation whose effect in the guest is
    /// unknown.
    pub fn uncertain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain: true,
        }
    }
}

impl From<std::io::Error> for OpError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Convenience result alias for service operations.
pub type OpResult<T> = Result<T, OpError>;
