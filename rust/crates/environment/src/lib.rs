//! Selected-environment resolution and startup ownership.
//!
//! This crate is the Rust port of `bin/environment_config.py` and
//! `bin/environment_runtime.py`. It validates and canonicalizes an explicitly
//! selected environment profile (never falling back to legacy defaults), hashes
//! that profile into a cross-language identity, derives the exported
//! environment variables, and provides an RAII guard that holds the two daemon
//! `flock`s and binds both environment markers.

mod config;
pub mod python_json;
mod runtime;

pub use config::{canonical_profile, load_environment, load_environment_with, profile_identity};
pub use runtime::{ownership, Ownership};

/// Name of the per-root marker file binding a root to an environment identity.
pub const MARKER: &str = runtime::MARKER;

/// An explicitly selected environment is invalid, or ownership of its roots
/// cannot be established. Never fall back to legacy defaults.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentError {
    /// A validation failure. The [`Display`](std::fmt::Display) text matches the
    /// Python `EnvironmentError` message for the same failure.
    #[error("{0}")]
    Invalid(String),
}

impl EnvironmentError {
    pub(crate) fn msg(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

// Re-exported so the guard's lock/marker behaviour stays testable from outside
// the crate without widening the public surface beyond the contract.
#[doc(hidden)]
pub use runtime::STORE_LOCK;
