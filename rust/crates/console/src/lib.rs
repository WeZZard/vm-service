//! The guest-console controller subsystem of `vm-service`.
//!
//! This crate owns the trusted viewer configuration (`config`), the fixed guest
//! agent commands (`guest`), and the lease-bound controller (`sessions`). It
//! does not depend on `vm-service-core`.

pub mod config;
pub mod guest;
pub mod sessions;
