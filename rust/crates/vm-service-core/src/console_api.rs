//! One adapter for the `console` crate's public surface.
//!
//! The console subsystem is ported in its own crate. Routing every call through
//! this module means a module-path change is a one-file edit here.
//!
//! The [`ConsoleController`] trait is the injection seam for the console
//! manager, mirroring the [`crate::host::Host`] seam: production code binds the
//! real [`Manager`], and tests substitute an in-memory controller without
//! touching the lexically surrounding service code. It is always compiled so
//! integration tests can reach it, but the default binding is always the real
//! manager.

use std::path::Path;
use std::time::Instant;

use serde_json::{Map, Value};

pub use console::config::{availability, load_config, ConsoleConfig, ConsoleConfigError};
pub use console::guest::{pending_codes, probe_command, stream_command};
pub use console::sessions::{identity, ConsoleError, Manager};

/// The console-manager operations the service performs on a lease.
///
/// This is the console analogue of [`crate::host::Host`]: the production
/// binding is [`Manager`], and tests may bind a recording or scripted
/// implementation. Every method maps one-to-one onto the existing [`Manager`]
/// method of the same name.
pub trait ConsoleController: Send + Sync {
    /// The advertised console capabilities, without probing anything.
    fn capabilities(&self) -> Value;
    /// Require a console kind to be configured before allocation.
    fn require_available(&self, kind: &str) -> Result<(), ConsoleError>;
    /// Reserve a session inside the lease reservation transaction.
    fn reserve(&self, record: &Map<String, Value>) -> Result<(), ConsoleError>;
    /// Bind the guest's existing console and return its report.
    fn prepare(
        &self,
        record: &Map<String, Value>,
        key_dir: &Path,
        timeout_s: u64,
    ) -> Result<Map<String, Value>, ConsoleError>;
    /// Revoke the session bound to a record's lease.
    fn revoke(&self, record: &Map<String, Value>, reason: &str);
    /// Publish a renewed deadline to every live attempt.
    fn renew(&self, record: &Map<String, Value>, deadline: Option<Instant>);
    /// Forget a VM's session and revoke it.
    fn forget(&self, vm: &str);
    /// Resolve a session report without requiring it to be currently active.
    fn resolve(
        &self,
        record: &Map<String, Value>,
        lease_id: Option<&str>,
    ) -> Result<Map<String, Value>, ConsoleError>;
    /// Open a viewer attempt and dispatch its configuration to a worker.
    fn open(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError>;
    /// Cancel an attempt, creating a closed placeholder if it never existed.
    fn cancel(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError>;
    /// Revoke every session and mark the controller closed.
    fn shutdown(&self);
}

impl ConsoleController for Manager {
    fn capabilities(&self) -> Value {
        Manager::capabilities(self)
    }

    fn require_available(&self, kind: &str) -> Result<(), ConsoleError> {
        Manager::require_available(self, kind)
    }

    fn reserve(&self, record: &Map<String, Value>) -> Result<(), ConsoleError> {
        Manager::reserve(self, record)
    }

    fn prepare(
        &self,
        record: &Map<String, Value>,
        key_dir: &Path,
        timeout_s: u64,
    ) -> Result<Map<String, Value>, ConsoleError> {
        Manager::prepare(self, record, key_dir, timeout_s)
    }

    fn revoke(&self, record: &Map<String, Value>, reason: &str) {
        Manager::revoke(self, record, reason);
    }

    fn renew(&self, record: &Map<String, Value>, deadline: Option<Instant>) {
        Manager::renew(self, record, deadline);
    }

    fn forget(&self, vm: &str) {
        Manager::forget(self, vm);
    }

    fn resolve(
        &self,
        record: &Map<String, Value>,
        lease_id: Option<&str>,
    ) -> Result<Map<String, Value>, ConsoleError> {
        Manager::resolve(self, record, lease_id)
    }

    fn open(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        Manager::open(self, record, lease_id, console_id, attempt_id)
    }

    fn cancel(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        Manager::cancel(self, record, lease_id, console_id, attempt_id)
    }

    fn shutdown(&self) {
        Manager::shutdown(self);
    }
}
