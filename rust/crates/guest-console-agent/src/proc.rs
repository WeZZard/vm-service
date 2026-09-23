//! Subprocess, identity and clock helpers shared by the Linux and macOS
//! inspection paths. Mirrors `bin/guest-console-agent.py`'s `run`, `identity`
//! and the module-level `DEADLINE`.

use std::ffi::CStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::RefCell;

use crate::api_time;
use crate::signal;
use crate::Unavailable;

/// The environment every inspected subprocess receives. Python passes this
/// explicitly, so the child never inherits the caller's environment.
pub const SAFE_ENV: [(&str, &str); 3] = [
    ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
    ("LANG", "C"),
    ("LC_ALL", "C"),
];

const SUBPROCESS_TIMEOUT_SECS: f64 = 5.0;
const MAX_SUBPROCESS_OUTPUT: usize = 4 * 1024 * 1024;

/// The monotonic deadline set by `serve` after configuration validation. Also
/// bounds subprocesses used during periodic validation.
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Test-only seams
// ---------------------------------------------------------------------------
//
// The Python unit tests `mock.patch.object(guest, "run", ...)`,
// `mock.patch.object(guest, "identity", ...)`, `mock.patch.object(guest.os,
// "stat", ...)` and `mock.patch.object(guest.Path, "exists", ...)` to drive
// the macOS inspection paths without a real macOS host. The Rust translation
// cannot patch the process-global `subprocess` module, so each seam is a
// per-thread override consulted by the production helper. The overrides live
// only in test builds; with none installed the production behavior is exactly
// as before.

/// A fake `run(argv, ...)` result: raw stdout on success, or the fixed
/// diagnostic the mocked call raised. It replaces the wrapper function, so it
/// also observes the `extra_env` the caller passed.
#[cfg(test)]
pub(crate) type TestRunner =
    Box<dyn FnMut(&[&str], Option<&[(&str, &str)]>) -> Result<Vec<u8>, Unavailable>>;

/// A fake `subprocess.run(argv, ...)` outcome: process exit status plus raw
/// stdout. Unlike [`TestRunner`] this models the process boundary, so the real
/// return-code and output-length checks still run exactly as in production.
#[cfg(test)]
pub(crate) type TestProcess = Box<dyn FnMut(&[&str]) -> (i32, Vec<u8>)>;

#[cfg(test)]
thread_local! {
    static TEST_RUNNER: RefCell<Option<TestRunner>> = const { RefCell::new(None) };
    static TEST_PROCESS: RefCell<Option<TestProcess>> = const { RefCell::new(None) };
    static TEST_IDENTITY: RefCell<Option<(libc::uid_t, String)>> = const { RefCell::new(None) };
    static TEST_CONSOLE_UID: RefCell<Option<libc::uid_t>> = const { RefCell::new(None) };
    static TEST_REMOTE_MANAGEMENT: RefCell<Option<bool>> = const { RefCell::new(None) };
    static TEST_PROC_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Install or clear the fake command runner for the current thread.
#[cfg(test)]
pub(crate) fn set_test_runner(runner: Option<TestRunner>) {
    TEST_RUNNER.with(|cell| *cell.borrow_mut() = runner);
}

#[cfg(test)]
fn test_runner(
    argv: &[&str],
    env: Option<&[(&str, &str)]>,
) -> Option<Result<Vec<u8>, Unavailable>> {
    TEST_RUNNER.with(|cell| cell.borrow_mut().as_mut().map(|runner| runner(argv, env)))
}

/// Install or clear the fake raw-process outcome for the current thread.
#[cfg(test)]
pub(crate) fn set_test_process(process: Option<TestProcess>) {
    TEST_PROCESS.with(|cell| *cell.borrow_mut() = process);
}

#[cfg(test)]
fn test_process(argv: &[&str]) -> Option<(i32, Vec<u8>)> {
    TEST_PROCESS.with(|cell| cell.borrow_mut().as_mut().map(|process| process(argv)))
}

/// Install or clear the `/proc` root override for the current thread.
#[cfg(test)]
pub(crate) fn set_test_proc_root(root: Option<PathBuf>) {
    TEST_PROC_ROOT.with(|cell| *cell.borrow_mut() = root);
}

/// The root under which `/proc` reads are resolved. Pointing it at a temporary
/// tree lets the Linux session tests supply their own `environ` and `boot_id`.
pub(crate) fn procfs_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_PROC_ROOT.with(|cell| cell.borrow().clone()) {
        return root;
    }
    PathBuf::from("/proc")
}

/// Install or clear the fake `identity` result for the current thread.
#[cfg(test)]
pub(crate) fn set_test_identity(identity: Option<(libc::uid_t, String)>) {
    TEST_IDENTITY.with(|cell| *cell.borrow_mut() = identity);
}

/// Install or clear the fake `/dev/console` owner for the current thread.
#[cfg(test)]
pub(crate) fn set_test_console_uid(uid: Option<libc::uid_t>) {
    TEST_CONSOLE_UID.with(|cell| *cell.borrow_mut() = uid);
}

/// The fake `/dev/console` owner, if the current thread installed one.
#[cfg(test)]
pub(crate) fn test_console_uid() -> Option<libc::uid_t> {
    TEST_CONSOLE_UID.with(|cell| *cell.borrow())
}

/// Install or clear the fake Remote Management marker presence.
#[cfg(test)]
pub(crate) fn set_test_remote_management(present: Option<bool>) {
    TEST_REMOTE_MANAGEMENT.with(|cell| *cell.borrow_mut() = present);
}

/// The fake Remote Management marker presence, if one was installed.
#[cfg(test)]
pub(crate) fn test_remote_management() -> Option<bool> {
    TEST_REMOTE_MANAGEMENT.with(|cell| *cell.borrow())
}

/// The current `DEADLINE`, if `serve` has installed one.
pub fn current_deadline() -> Option<Instant> {
    DEADLINE.lock().map(|guard| *guard).unwrap_or(None)
}

/// Install or clear the global subprocess deadline.
pub fn set_deadline(value: Option<Instant>) {
    if let Ok(mut guard) = DEADLINE.lock() {
        *guard = value;
    }
}

/// `Unavailable("deadline_expired")` when `DEADLINE` has already passed.
fn deadline_expired() -> Unavailable {
    Unavailable::new("deadline_expired")
}

/// Run a subprocess and decode stdout as UTF-8, mirroring Python's text mode.
pub fn run_text(
    argv: &[&str],
    extra_env: Option<&[(&str, &str)]>,
    allowed_returncodes: &[i32],
) -> Result<String, Unavailable> {
    let bytes = run_core(argv, extra_env, allowed_returncodes)?;
    String::from_utf8(bytes).map_err(|_| Unavailable::new("inspection_failed"))
}

/// Run a subprocess and return raw stdout, mirroring Python's `binary=True`.
pub fn run_bytes(
    argv: &[&str],
    extra_env: Option<&[(&str, &str)]>,
    allowed_returncodes: &[i32],
) -> Result<Vec<u8>, Unavailable> {
    run_core(argv, extra_env, allowed_returncodes)
}

fn run_core(
    argv: &[&str],
    extra_env: Option<&[(&str, &str)]>,
    allowed_returncodes: &[i32],
) -> Result<Vec<u8>, Unavailable> {
    #[cfg(test)]
    if let Some(result) = test_runner(argv, extra_env) {
        return result;
    }
    // A fake process outcome still runs through the production return-code and
    // output-length checks, so acceptance of exit status 1 is narrowly scoped.
    #[cfg(test)]
    if let Some((code, stdout)) = test_process(argv) {
        if !allowed_returncodes.contains(&code) || stdout.len() > MAX_SUBPROCESS_OUTPUT {
            return Err(Unavailable::new("inspection_failed"));
        }
        return Ok(stdout);
    }
    if argv.is_empty() {
        return Err(Unavailable::new("inspection_failed"));
    }
    let mut timeout = SUBPROCESS_TIMEOUT_SECS;
    if let Some(deadline) = current_deadline() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        if remaining < timeout {
            timeout = remaining;
        }
    }
    if timeout <= 0.0 {
        return Err(deadline_expired());
    }

    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]);
    command.env_clear();
    for (key, value) in SAFE_ENV {
        command.env(key, value);
    }
    if let Some(extra) = extra_env {
        for (key, value) in extra {
            command.env(key, value);
        }
    }
    // Python does not set stdin when `input is None`, so the child inherits the
    // parent's stdin. None of the inspection commands read it.
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|_| Unavailable::new("inspection_failed"))?;
    let stdout_reader = child.stdout.take().map(|mut stream| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stream.read_to_end(&mut buffer);
            buffer
        })
    });

    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let status: Option<ExitStatus> = loop {
        if signal::interrupted() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Unavailable::new("interrupted"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let stdout = match stdout_reader {
        Some(handle) => handle.join().unwrap_or_default(),
        None => Vec::new(),
    };
    let status = match status {
        Some(status) => status,
        None => return Err(Unavailable::new("inspection_failed")),
    };
    let code = status.code();
    if !allowed_returncodes
        .iter()
        .any(|allowed| Some(*allowed) == code)
    {
        return Err(Unavailable::new("inspection_failed"));
    }
    if stdout.len() > MAX_SUBPROCESS_OUTPUT {
        return Err(Unavailable::new("inspection_failed"));
    }
    Ok(stdout)
}

/// Wait up to `timeout` for `child` to exit, returning its status if it did.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The guest user running the agent.
///
/// `pwd.getpwuid(uid).pw_name` is replaced by `libc::getpwuid_r`; a missing
/// entry is reported as `inspection_failed`, which is what Python's uncaught
/// `KeyError` becomes in the probe.
pub fn identity() -> Result<(libc::uid_t, String), Unavailable> {
    #[cfg(test)]
    if let Some(value) = TEST_IDENTITY.with(|cell| cell.borrow().clone()) {
        return Ok(value);
    }
    // SAFETY: `getuid`/`geteuid` take no arguments and cannot fail.
    let uid = unsafe { libc::getuid() };
    let euid = unsafe { libc::geteuid() };
    identity_for(uid, euid)
}

/// The real `identity` decision with the uids injected, so the rejection rule
/// can be unit tested without a process-global seam.
///
/// Python's `test_root_and_mismatched_effective_uid_are_rejected` patches
/// `os.getuid`/`os.geteuid`; this is the pure equivalent it drives.
pub(crate) fn identity_for(
    uid: libc::uid_t,
    euid: libc::uid_t,
) -> Result<(libc::uid_t, String), Unavailable> {
    if uid == 0 || euid != uid {
        return Err(Unavailable::new("console_user_required"));
    }
    match username(uid) {
        Some(name) => Ok((uid, name)),
        None => Err(Unavailable::new("inspection_failed")),
    }
}

fn username(uid: libc::uid_t) -> Option<String> {
    // SAFETY: `getpwuid_r` writes into the buffers and result pointer provided
    // here; all of them live for the duration of the call.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let code = libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if code != 0 || result.is_null() || entry.pw_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(entry.pw_name);
        Some(name.to_string_lossy().into_owned())
    }
}

/// Unix seconds as `f64`, matching Python `time.time()`.
pub fn unix_time() -> f64 {
    api_time::unix_seconds()
}

/// Monotonic seconds as `f64`, matching Python `time.monotonic()`.
pub fn monotonic() -> f64 {
    api_time::monotonic_seconds()
}
