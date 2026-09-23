//! Signal handling for the `serve` operation.
//!
//! Python installs a handler for `SIGHUP`, `SIGTERM` and `SIGINT` that raises
//! `Unavailable("interrupted")`. A Rust signal handler cannot raise, so the
//! handler records the signal and the blocking loops poll the flag and return
//! the same diagnostic.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::Unavailable;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// True once delivery of a handled signal has been observed.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// `Err(Unavailable("interrupted"))` when a handled signal has arrived.
pub fn check_interrupted() -> Result<(), Unavailable> {
    if interrupted() {
        Err(Unavailable::new("interrupted"))
    } else {
        Ok(())
    }
}

extern "C" fn handle_signal(_signum: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install the `SIGHUP`/`SIGTERM`/`SIGINT` handlers.
///
/// The disposition carries no `SA_RESTART`, so a blocking `poll` is interrupted
/// and the loops re-check the flag, matching the Python handler's immediacy.
pub fn install_signal_handlers() {
    // SAFETY: `handle_signal` is an `extern "C"` function with the signal
    // handler signature; `sigemptyset` and `sigaction` are async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}
