//! Live host-name observation through System Configuration with a periodic
//! re-read on the same path (MI-2, DC-88).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use machine_service_core::service::Service;

/// Keeps the observer and its advertisement alive until the server stops.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Observe notifications and re-read every 30 seconds even if none arrive.
/// Tests shorten only this fallback and supply the file-backed name source.
pub fn start(service: Arc<Service>, mode: discovery::Mode) -> Result<Watcher, String> {
    discovery::dnssd::Scope::from_environment()?;
    let interval = match std::env::var("MACHINE_SERVICE_NAME_POLL_MS") {
        Ok(value) => Duration::from_millis(
            value
                .parse::<u64>()
                .ok()
                .filter(|ms| *ms > 0)
                .ok_or("MACHINE_SERVICE_NAME_POLL_MS must be a positive integer")?,
        ),
        Err(std::env::VarError::NotPresent) => Duration::from_secs(30),
        Err(_) => return Err("MACHINE_SERVICE_NAME_POLL_MS is not valid UTF-8".into()),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("machine-name-watch".into())
        .spawn(move || {
            let notifications = mac::Notifications::new();
            if notifications.is_none() {
                service.log(
                    "WARN: host-name notifications unavailable; using periodic re-read (MI-2)",
                );
            }
            let mut advertisement = super::advertise(&service, mode);
            let mut deadline = Instant::now() + interval;
            while !flag.load(Ordering::Relaxed) {
                let wait = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(250));
                let notified = match &notifications {
                    Some(notifications) => notifications.wait(wait),
                    None => {
                        std::thread::sleep(wait);
                        false
                    }
                };
                if !notified && Instant::now() < deadline {
                    continue;
                }
                deadline = Instant::now() + interval;
                match super::durable_routes::reread_name() {
                    Ok(Some((old, new))) => {
                        drop(advertisement.take());
                        advertisement = super::advertise(&service, mode);
                        service.log(&format!(
                            "machine name changed from {old} to {new} (MI-2, DC-88)"
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        service.log(&format!("WARN: cannot re-read machine name: {error}"))
                    }
                }
            }
            drop(advertisement);
        })
        .map_err(|error| format!("cannot watch machine name: {error}"))?;
    Ok(Watcher {
        stop,
        thread: Some(thread),
    })
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void};
    use std::ptr;
    use std::time::Duration;

    type Ref = *const c_void;
    type Callback = extern "C" fn(Ref, Ref, *mut c_void);

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCreate(
            allocator: Ref,
            name: Ref,
            callback: Option<Callback>,
            context: *const c_void,
        ) -> Ref;
        fn SCDynamicStoreKeyCreateHostNames(allocator: Ref) -> Ref;
        fn SCDynamicStoreSetNotificationKeys(store: Ref, keys: Ref, patterns: Ref) -> u8;
        fn SCDynamicStoreCreateRunLoopSource(allocator: Ref, store: Ref, order: isize) -> Ref;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ref;
        fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
        fn CFArrayCreate(
            allocator: Ref,
            values: *const Ref,
            count: isize,
            callbacks: *const c_void,
        ) -> Ref;
        fn CFRelease(value: Ref);
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: u8) -> i32;
    }

    struct Owned(Ref);
    impl Owned {
        fn new(value: Ref) -> Option<Self> {
            (!value.is_null()).then_some(Self(value))
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: each Owned comes from one successful CF Create call.
            unsafe { CFRelease(self.0) };
        }
    }

    // The run loop returning after this source wakes the shared re-read path.
    extern "C" fn changed(_: Ref, _: Ref, _: *mut c_void) {}

    /// Objects stay on the observer thread and are released there.
    pub struct Notifications {
        source: Owned,
        _store: Owned,
        _keys: Owned,
        _key: Owned,
    }

    impl Notifications {
        pub fn new() -> Option<Self> {
            // SAFETY: the Create calls use default allocators and valid CF
            // values. The callback has no context; no Rust reference crosses
            // FFI. The temporary array borrows `key`, which outlives its use.
            unsafe {
                let name = Owned::new(CFStringCreateWithCString(
                    ptr::null(),
                    c"machine-service-name".as_ptr(),
                    0x08000100,
                ))?;
                let store = Owned::new(SCDynamicStoreCreate(
                    ptr::null(),
                    name.0,
                    Some(changed),
                    ptr::null(),
                ))?;
                let key = Owned::new(SCDynamicStoreKeyCreateHostNames(ptr::null()))?;
                let keys = Owned::new(CFArrayCreate(ptr::null(), &key.0, 1, ptr::null()))?;
                if SCDynamicStoreSetNotificationKeys(store.0, keys.0, ptr::null()) == 0 {
                    return None;
                }
                let source =
                    Owned::new(SCDynamicStoreCreateRunLoopSource(ptr::null(), store.0, 0))?;
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source.0, kCFRunLoopDefaultMode);
                Some(Self {
                    source,
                    _store: store,
                    _keys: keys,
                    _key: key,
                })
            }
        }

        pub fn wait(&self, timeout: Duration) -> bool {
            // SAFETY: source is registered on this thread's current run loop
            // for this object's lifetime. Four means a source was handled.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, timeout.as_secs_f64(), 1) == 4 }
        }
    }

    impl Drop for Notifications {
        fn drop(&mut self) {
            // SAFETY: remove the live source on the same thread before either
            // owned CF object is released.
            unsafe {
                CFRunLoopRemoveSource(CFRunLoopGetCurrent(), self.source.0, kCFRunLoopDefaultMode)
            };
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    pub struct Notifications;
    impl Notifications {
        pub fn new() -> Option<Self> {
            None
        }
        pub fn wait(&self, _: std::time::Duration) -> bool {
            false
        }
    }
}
