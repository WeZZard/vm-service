//! The file watcher that starts a catalog rebuild early (IC-3, DC-38).
//!
//! The watcher only accelerates: the request-time identity comparison is what
//! keeps the catalog correct, so a missed or merged event costs one request's
//! wait and never a stale answer. On macOS it uses kqueue vnode events
//! through `libc`, which the crate already depends on, instead of adding a
//! file-watching crate: the inputs are one folder, its subfolders and one
//! `line.conf` in each, which a handful of descriptors cover. Elsewhere it
//! does nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::ImageCatalog;

/// How long the watcher gathers events after the first one, so that a
/// burst of writes starts one rebuild.
const SETTLE: Duration = Duration::from_millis(100);
/// How often the watcher checks whether it should stop, and retries a
/// folder that does not exist.
const TICK: Duration = Duration::from_millis(250);

/// A running watcher. Dropping it stops the thread.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Watch `catalog`'s source and, on any change, read the catalog without
/// waiting, which starts a rebuild when the inputs changed. Returns `None`
/// when the source has nothing to watch.
pub fn watch_catalog(catalog: &Arc<ImageCatalog>) -> std::io::Result<Option<Watcher>> {
    let Some(root) = catalog.source().watch_root() else {
        return Ok(None);
    };
    let catalog = Arc::clone(catalog);
    watch(root, move || {
        catalog.peek();
    })
    .map(Some)
}

/// Call `on_change` after files change under `root`: the folder itself, its
/// subfolders, and each subfolder's `line.conf`.
pub fn watch(root: PathBuf, on_change: impl Fn() + Send + 'static) -> std::io::Result<Watcher> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("image-catalog-watch".into())
        .spawn(move || run(&root, &on_change, &flag))?;
    Ok(Watcher {
        stop,
        thread: Some(thread),
    })
}

#[cfg(not(target_os = "macos"))]
fn run(_root: &Path, _on_change: &dyn Fn(), _stop: &AtomicBool) {}

#[cfg(target_os = "macos")]
fn run(root: &Path, on_change: &dyn Fn(), stop: &AtomicBool) {
    let mut watched_before = true;
    while !stop.load(Ordering::SeqCst) {
        let Some(queue) = kqueue::Queue::new() else {
            std::thread::sleep(TICK);
            continue;
        };
        let files = kqueue::open_watch_set(root);
        if files.is_empty() {
            // The folder is missing; look again shortly.
            watched_before = false;
            std::thread::sleep(TICK);
            continue;
        }
        if !watched_before {
            // The folder appeared since the last look.
            watched_before = true;
            on_change();
        }
        if !queue.register(&files) {
            std::thread::sleep(TICK);
            continue;
        }
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match queue.wait(TICK) {
                kqueue::Wait::Event => {
                    // Gather the rest of a burst, then re-arm on the files
                    // as they are now, since an editor may have replaced them.
                    while let kqueue::Wait::Event = queue.wait(SETTLE) {}
                    on_change();
                    break;
                }
                kqueue::Wait::Timeout => {}
                kqueue::Wait::Failed => break,
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod kqueue {
    //! Thin, commented `libc` wrappers around kqueue vnode events.

    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::time::Duration;

    /// The vnode events that mean a watched file or folder changed.
    const EVENTS: u32 = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_ATTRIB
        | libc::NOTE_LINK
        | libc::NOTE_REVOKE;

    pub(super) struct Queue(OwnedFd);

    pub(super) enum Wait {
        Event,
        Timeout,
        Failed,
    }

    impl Queue {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: `kqueue` takes no arguments and returns a new
            // descriptor or -1.
            let fd = unsafe { libc::kqueue() };
            // SAFETY: a nonnegative `fd` is a descriptor this call owns.
            (fd >= 0).then(|| Queue(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// Register a vnode filter for every file. The files must outlive
        /// the registration, which `run` guarantees by holding them.
        pub(super) fn register(&self, files: &[File]) -> bool {
            let changes: Vec<libc::kevent> = files
                .iter()
                .map(|file| libc::kevent {
                    ident: file.as_raw_fd() as usize,
                    filter: libc::EVFILT_VNODE,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    fflags: EVENTS,
                    data: 0,
                    udata: std::ptr::null_mut(),
                })
                .collect();
            // SAFETY: `changes` is a valid array of `changes.len()` events;
            // no event list is requested, so no output buffer is written.
            let result = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    changes.as_ptr(),
                    changes.len() as libc::c_int,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            result >= 0
        }

        pub(super) fn wait(&self, timeout: Duration) -> Wait {
            let mut events: [libc::kevent; 16] =
                // SAFETY: `kevent` is plain data, valid when zeroed.
                unsafe { std::mem::zeroed() };
            let spec = libc::timespec {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_nsec: timeout.subsec_nanos() as libc::c_long,
            };
            // SAFETY: `events` has room for 16 events, `spec` is a valid
            // timeout, and no changes are submitted.
            let count = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    &spec,
                )
            };
            match count {
                0 => Wait::Timeout,
                n if n > 0 => Wait::Event,
                _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                    Wait::Timeout
                }
                _ => Wait::Failed,
            }
        }
    }

    fn open_event_only(path: &Path) -> Option<File> {
        // `O_EVTONLY` asks for events only, so the descriptor never keeps a
        // volume from unmounting.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_EVTONLY)
            .open(path)
            .ok()
    }

    /// Open the folder, each subfolder and each subfolder's `line.conf`.
    /// Empty when the folder cannot be opened.
    pub(super) fn open_watch_set(root: &Path) -> Vec<File> {
        let Some(folder) = open_event_only(root) else {
            return Vec::new();
        };
        let mut files = vec![folder];
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(open_event_only(&path));
                    files.extend(open_event_only(&path.join("line.conf")));
                }
            }
        }
        files
    }
}
