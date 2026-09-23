//! A reentrant mutex, matching Python's `threading.RLock`.
//!
//! The Python daemon serializes operations on one VM with
//! `_operation_locks.setdefault(vm, threading.RLock())`, so a path that already
//! holds the lock may acquire it again: `release` wraps `_begin_release`, and
//! `acquire` rolls a failed provisioning back through `_begin_release` while
//! still holding it. `std::sync::Mutex` is not reentrant, so the port needs that
//! guarantee explicitly rather than by accident.

use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

/// A mutex the owning thread may acquire repeatedly.
#[derive(Debug, Default)]
pub struct ReentrantLock {
    state: Mutex<Option<(ThreadId, usize)>>,
    ready: Condvar,
}

impl ReentrantLock {
    /// Create an unlocked reentrant mutex.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    /// Acquire the lock, blocking only while another thread holds it.
    ///
    /// Re-acquisition by the owning thread never blocks, mirroring `RLock`.
    pub fn lock(&self) -> ReentrantGuard<'_> {
        let current = thread::current().id();
        let mut state = self.state.lock().expect("reentrant lock state");
        loop {
            match *state {
                None => {
                    *state = Some((current, 1));
                    break;
                }
                Some((owner, depth)) if owner == current => {
                    *state = Some((owner, depth + 1));
                    break;
                }
                Some(_) => {
                    state = self.ready.wait(state).expect("reentrant lock wait");
                }
            }
        }
        ReentrantGuard {
            lock: self,
            thread: current,
        }
    }
}

/// One held level of a [`ReentrantLock`].
///
/// Dropping the guard releases one level; the lock becomes available to other
/// threads only when the outermost guard is dropped.
#[derive(Debug)]
pub struct ReentrantGuard<'a> {
    lock: &'a ReentrantLock,
    thread: ThreadId,
}

impl Drop for ReentrantGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.lock.state.lock().expect("reentrant lock state");
        if let Some((owner, depth)) = *state {
            if owner != self.thread {
                return;
            }
            if depth <= 1 {
                *state = None;
                drop(state);
                self.lock.ready.notify_one();
                return;
            }
            *state = Some((owner, depth - 1));
        }
    }
}
