//! Clock helpers. Times are Unix seconds as `f64`; monotonic deadlines use
//! `Instant`.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static START: OnceLock<Instant> = OnceLock::new();

fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

/// Unix seconds as `f64`, matching Python `time.time()`.
pub fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// Monotonic seconds as `f64`, matching Python `time.monotonic()`.
pub fn monotonic_seconds() -> f64 {
    start().elapsed().as_secs_f64()
}

/// Convert an absolute monotonic-seconds value into an `Instant`.
///
/// `offset` is measured on the same monotonic scale as [`monotonic_seconds`].
pub fn monotonic_instant(offset: f64, now: f64) -> Instant {
    Instant::now() + std::time::Duration::from_secs_f64((offset - now).max(0.0))
}
