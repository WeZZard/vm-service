//! Guest-only console adapter. The host embeds this binary; no guest
//! installation is needed.
//!
//! This is the Rust port of `bin/guest-console-agent.py`. The Python source is
//! the behavioral specification: every diagnostic code string, probe field,
//! stdin framing rule, subprocess timeout rule and exit code is preserved.

pub mod api_time;
pub mod config;
pub mod linux;
pub mod macos;
pub mod proc;
pub mod python;
pub mod relay;
pub mod serve;
pub mod signal;

#[cfg(test)]
mod tests;

/// Protocol version emitted in the probe record.
pub const VERSION: i64 = 1;

/// Guest console family selected on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Linux,
    Macos,
}

impl Kind {
    /// Parse the CLI spelling, matching the Python `("linux", "macos")` tuple.
    pub fn parse(value: &str) -> Option<Kind> {
        match value {
            "linux" => Some(Kind::Linux),
            "macos" => Some(Kind::Macos),
            _ => None,
        }
    }

    /// The CLI spelling of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Linux => "linux",
            Kind::Macos => "macos",
        }
    }
}

/// A fixed diagnostic code chosen by the guest agent.
///
/// The payload is always one of the agent's own constant strings, never
/// subprocess output. `Display` prints the bare code exactly as Python's
/// `str(Unavailable(code))` does.
///
/// `unexpected` marks the Rust equivalent of a Python exception that is *not*
/// `Unavailable`: Python's `probe` reports such a failure as
/// `inspection_failed`, while `main` reports it as `stream_failed`. Keeping
/// the distinction on the error lets each entry point choose the same outer
/// code Python does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}")]
pub struct Unavailable {
    code: String,
    unexpected: bool,
}

impl Unavailable {
    /// Build a diagnostic from a fixed code.
    pub fn new(code: impl Into<String>) -> Self {
        Unavailable {
            code: code.into(),
            unexpected: false,
        }
    }

    /// The Rust equivalent of a Python exception that escaped `Unavailable`.
    ///
    /// `code()` still returns `inspection_failed`, which is what Python's
    /// `probe` emits; `serve`'s caller uses [`Self::is_unexpected`] to emit
    /// `stream_failed` instead.
    pub fn unexpected() -> Self {
        Unavailable {
            code: "inspection_failed".to_string(),
            unexpected: true,
        }
    }

    /// The bare diagnostic code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Whether this error represents an uncaught Python exception rather than
    /// an explicit `Unavailable`.
    pub fn is_unexpected(&self) -> bool {
        self.unexpected
    }
}
