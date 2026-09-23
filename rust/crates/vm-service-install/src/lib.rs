//! Install and remove the `com.wezzard.vm-service` launchd agent.
//!
//! This crate is the Rust port of `bin/install-vm-service.sh`. The shell script
//! embedded a Python program that validated the selected environment and the
//! console configuration with `environment_config` and `console_config`; this
//! port performs the same checks through the [`environment`] and [`console`]
//! workspace crates.
//!
//! Every refusal prints `vm-service installation refused: <reason>` on stderr
//! and exits `1`, matching the Python wrapper.

pub mod installer;
pub mod plist;

pub use installer::{main_entry, Action, Cli, InstallError};
