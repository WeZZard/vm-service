//! The guest session: one long-lived, checksummed byte stream between
//! machine-service and a helper in the guest, carrying many execs, writes and reads.
//!
//! The design is `docs/arch/guest-session.md` (GS-1 to GS-6). This crate holds
//! the frame codec ([`frame`]), the guest-side helper's request loop
//! ([`agent`]), and the host-side client with its failure outcomes
//! ([`client`]). The `guest-session-agent` binary runs [`agent::serve`] on its
//! standard input and output.

pub mod agent;
pub mod client;
mod crc;
pub mod frame;

pub use crc::crc32;

#[cfg(test)]
mod tests;
