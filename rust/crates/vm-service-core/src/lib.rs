//! `vm-service` core: lease state, Tart lifecycle, SSH access, and the HTTP
//! operations the daemon exposes.

pub mod config;
pub mod console_api;
pub mod error;
pub mod host;
pub mod lines;
pub mod ops;
pub mod packs;
pub mod proc;
pub mod python;
pub mod reentrant;
pub mod service;
pub mod snapshot;
pub mod ssh;
pub mod state;
pub mod tart;

/// Python-compatible JSON number handling, re-exported for the daemon.
pub use environment::python_json;

/// The automatic garbage-collection interval, in seconds.
pub const GC_INTERVAL_S: u64 = 60;

// Re-export the sibling crates the daemon consumes, so `vm-service` needs only
// one dependency.
pub use acquisition_options;
pub use application_catalog;
pub use console as console_crate;
pub use control_only;
pub use environment;
pub use lease_keys;

pub use error::{OpError, OpResult};
pub use host::{BootProcess, Host, RealHost, TartOutput};
pub use service::Service;
