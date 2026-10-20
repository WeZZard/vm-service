//! The guest-console controller subsystem of `machine-service`.
//!
//! This crate owns the trusted viewer configuration (`config`), the fixed guest
//! agent commands (`guest`), and the lease-bound controller (`sessions`). It
//! does not depend on `machine-service-core`.

pub mod config;
pub mod guest;
pub mod sessions;
