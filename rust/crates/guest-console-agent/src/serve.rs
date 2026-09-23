//! `probe`, `serve` and CLI dispatch. Mirrors the `probe`, `serve` and `main`
//! functions of `bin/guest-console-agent.py`.

use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
#[cfg(test)]
use std::os::fd::RawFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::{Cell, RefCell};

use serde_json::{Map, Value};

use crate::config;
use crate::linux;
use crate::macos;
use crate::proc;
use crate::relay;
use crate::signal;
use crate::{Kind, Unavailable, VERSION};

/// Dispatch `ready` to the inspected platform; a mismatch is reported without
/// touching any guest service.
pub fn ready(kind: Kind) -> Result<(Value, Value), Unavailable> {
    #[cfg(test)]
    if let Some(value) = TEST_READY.with(|cell| cell.borrow().clone()) {
        return value;
    }
    match kind {
        Kind::Linux if cfg!(target_os = "linux") => linux::linux_ready(),
        Kind::Macos if cfg!(target_os = "macos") => macos::mac_ready(),
        _ => Err(Unavailable::new("guest_platform_mismatch")),
    }
}

// ---------------------------------------------------------------------------
// Test-only seams
// ---------------------------------------------------------------------------
//
// Python's `serve` tests patch `guest.ready`, `guest.subprocess.Popen`,
// `guest.os.killpg` and `guest.relay`. Rust cannot patch process globals, so
// each is a per-thread override consulted by the production path. With none
// installed the production behavior is exactly as before.

/// The standard streams a test spawner is told the real child would receive.
/// Only the null choice is used by `serve_linux`; the type keeps the assertion
/// named after Python's `stderr == subprocess.DEVNULL` rather than a bare bool.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestStdio {
    /// A `/dev/null` stream, i.e. `subprocess.DEVNULL` / `Stdio::null()`.
    Null,
}

/// The spawn request a test spawner observes: the argv, environment and
/// standard streams the real x11vnc child is configured with. `stdin` and
/// `stdout` are the raw socketpair fds so a test can prove they are the same
/// socket, exactly as Python can prove `stdin is stdout`.
#[cfg(test)]
pub(crate) struct TestSpawnRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: RawFd,
    pub stdout: RawFd,
    pub stderr: TestStdio,
}

/// A fake `subprocess.Popen(...)`: `Ok(pid)` is the started process, `Err` is
/// the `OSError` Python raises when the executable cannot be started.
#[cfg(test)]
pub(crate) type TestSpawner = Box<dyn FnMut(TestSpawnRequest) -> Result<libc::pid_t, ()>>;

/// A fake `relay(...)`: the fixed diagnostic Python's patched relay returns.
#[cfg(test)]
pub(crate) type TestRelay = Box<dyn FnMut() -> Result<(), Unavailable>>;

#[cfg(test)]
thread_local! {
    static TEST_READY: RefCell<Option<Result<(Value, Value), Unavailable>>> =
        const { RefCell::new(None) };
    static TEST_SPAWNER: RefCell<Option<TestSpawner>> = const { RefCell::new(None) };
    static TEST_RELAY: RefCell<Option<TestRelay>> = const { RefCell::new(None) };
    static TEST_TERMINATIONS: RefCell<Vec<(libc::pid_t, libc::c_int)>> =
        const { RefCell::new(Vec::new()) };
    static TEST_KILLPG: Cell<bool> = const { Cell::new(false) };
}

/// Install or clear the result `ready` returns for the current thread.
#[cfg(test)]
pub(crate) fn set_test_ready(value: Option<Result<(Value, Value), Unavailable>>) {
    TEST_READY.with(|cell| *cell.borrow_mut() = value);
}

/// Install or clear the fake spawner. Installing one also diverts the
/// terminating guard's `killpg` calls into [`test_terminations`] so a fake pid
/// is never signalled for real.
#[cfg(test)]
pub(crate) fn set_test_spawner(spawner: Option<TestSpawner>) {
    TEST_KILLPG.with(|cell| cell.set(spawner.is_some()));
    TEST_SPAWNER.with(|cell| *cell.borrow_mut() = spawner);
}

/// Install or clear the fake relay.
#[cfg(test)]
pub(crate) fn set_test_relay(relay: Option<TestRelay>) {
    TEST_RELAY.with(|cell| *cell.borrow_mut() = relay);
}

/// The `(pid, signal)` pairs the terminating guard produced for this thread.
#[cfg(test)]
pub(crate) fn test_terminations() -> Vec<(libc::pid_t, libc::c_int)> {
    TEST_TERMINATIONS.with(|cell| cell.borrow().clone())
}

/// Clear the recorded terminations for this thread.
#[cfg(test)]
pub(crate) fn clear_test_terminations() {
    TEST_TERMINATIONS.with(|cell| cell.borrow_mut().clear());
}

fn observed_session(kind: Kind) -> Result<Value, Unavailable> {
    match kind {
        Kind::Linux => linux::linux_session(),
        Kind::Macos => macos::mac_session(),
    }
}

/// Build the probe record for `kind` by running the readiness check.
pub fn probe(kind: Kind) -> Value {
    probe_record(kind, ready(kind))
}

/// Build the probe record from an already-computed readiness outcome.
///
/// `outcome` is the agent's own result, so `error` is always one of the fixed
/// diagnostic codes and never subprocess output.
pub fn probe_record(kind: Kind, outcome: Result<(Value, Value), Unavailable>) -> Value {
    let linux = kind == Kind::Linux;
    let mut result = Map::new();
    result.insert("version".to_string(), Value::from(VERSION));
    result.insert("kind".to_string(), Value::String(kind.as_str().to_string()));
    result.insert("ready".to_string(), Value::Bool(false));
    result.insert("session".to_string(), Value::Null);
    result.insert(
        "backend".to_string(),
        Value::String(
            if linux {
                "x11vnc-inetd"
            } else {
                "apple-screen-sharing"
            }
            .to_string(),
        ),
    );
    result.insert(
        "authentication".to_string(),
        Value::String(
            if linux {
                "vnc-password"
            } else {
                "human-apple-account"
            }
            .to_string(),
        ),
    );
    result.insert("view_only".to_string(), Value::Bool(linux));
    result.insert(
        "acceptance".to_string(),
        Value::String("unverified".to_string()),
    );
    result.insert("error".to_string(), Value::Null);

    match outcome {
        Ok((session, isolation)) => {
            result.insert("session".to_string(), session);
            result.insert("isolation".to_string(), isolation);
            result.insert("ready".to_string(), Value::Bool(true));
        }
        Err(error) => {
            result.insert("error".to_string(), Value::String(error.code().to_string()));
        }
    }
    Value::Object(result)
}

/// The fixed loopback Screen Sharing endpoint. Python hardcodes
/// `socket.create_connection(("127.0.0.1", 5900))`; it is deliberately not a
/// caller-selectable host or port.
pub(crate) fn macos_endpoint_address() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 5900))
}

/// The stderr diagnostic `run_main` reports for a failed operation.
///
/// An explicit `Unavailable` keeps its own code; the Rust equivalent of any
/// other Python exception is reported as `stream_failed`, exactly as Python's
/// bare `except Exception` in `main` does.
pub(crate) fn failure_message(error: &Unavailable) -> String {
    if error.is_unexpected() {
        "stream_failed".to_string()
    } else {
        error.code().to_string()
    }
}

/// Relay RFB between standard input/output and the guest console endpoint.
pub fn serve(kind: Kind) -> Result<(), Unavailable> {
    let config = config::read_config(0)?;
    let deadline = config::validate_config(&config, kind)?;
    proc::set_deadline(Some(deadline));

    let (session, _isolation) = ready(kind)?;
    let expected = config.get("session").cloned().unwrap_or(Value::Null);
    if session != expected {
        return Err(Unavailable::new("session_changed"));
    }

    let expected_session = session.clone();
    let mut check = move || -> Result<(), Unavailable> {
        let observed = observed_session(kind)?;
        if observed != expected_session {
            return Err(Unavailable::new("session_changed"));
        }
        if kind == Kind::Macos {
            macos::mac_isolation()?;
            macos::mac_endpoint()?;
        }
        Ok(())
    };

    match kind {
        Kind::Macos => {
            let connect = |address: SocketAddr, timeout: Duration| {
                TcpStream::connect_timeout(&address, timeout)
                    .map_err(|_| Unavailable::new("stream_failed"))
            };
            serve_macos(deadline, &mut check, connect)?;
        }
        Kind::Linux => {
            serve_linux(&config, &session, deadline, &mut check)?;
        }
    }
    Ok(())
}

/// Connect to the fixed macOS loopback endpoint and relay the stream.
///
/// Factored out of [`serve`] so the connector is a parameter: Python's test
/// patches `socket.create_connection` to observe the address without a real
/// listener, and this is the equivalent seam.
pub(crate) fn serve_macos<E, C>(
    deadline: Instant,
    check: &mut dyn FnMut() -> Result<(), Unavailable>,
    connect: C,
) -> Result<(), Unavailable>
where
    C: FnOnce(SocketAddr, Duration) -> Result<E, Unavailable>,
    E: AsRawFd,
{
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64();
    let timeout = Duration::from_secs_f64(remaining.clamp(0.001, 5.0));
    let endpoint = connect(macos_endpoint_address(), timeout)?;
    check()?;
    run_relay(&endpoint, deadline, check)?;
    Ok(())
}

/// Kill the whole x11vnc process group when the guard is dropped, mirroring the
/// `finally` block of Python's `serve`.
///
/// The child is either a real process or a test double; both expose a pid so
/// the terminate sequence is identical apart from the test recording.
enum RunningChild {
    Real(Child),
    #[cfg(test)]
    Fake(libc::pid_t),
}

impl RunningChild {
    fn pid(&self) -> libc::pid_t {
        match self {
            RunningChild::Real(child) => child.id() as libc::pid_t,
            #[cfg(test)]
            RunningChild::Fake(pid) => *pid,
        }
    }
}

/// Whether a child has exited within `timeout`.
fn wait_for_child(child: &mut RunningChild, timeout: Duration) -> bool {
    match child {
        RunningChild::Real(child) => proc::wait_timeout(child, timeout).is_some(),
        #[cfg(test)]
        RunningChild::Fake(_) => true,
    }
}

/// Signal the process group `pid`, or record the signal when the spawn seam is
/// installed so a fake pid is never signalled for real.
fn terminate_process_group(pid: libc::pid_t, signal: libc::c_int) {
    #[cfg(test)]
    if TEST_KILLPG.with(|cell| cell.get()) {
        TEST_TERMINATIONS.with(|cell| cell.borrow_mut().push((pid, signal)));
        return;
    }
    // SAFETY: `killpg` takes the process group id and a signal number.
    unsafe {
        libc::killpg(pid, signal);
    }
}

struct ProcessGuard(Option<RunningChild>);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let child = match self.0.as_mut() {
            Some(child) => child,
            None => return,
        };
        let pid = child.pid();
        terminate_process_group(pid, libc::SIGTERM);
        if !wait_for_child(child, Duration::from_secs(2)) {
            terminate_process_group(pid, libc::SIGKILL);
            let _ = wait_for_child(child, Duration::from_secs(2));
        }
    }
}

/// Relay through the real bounded relay, or through the test override.
fn run_relay<E: AsRawFd>(
    endpoint: &E,
    deadline: Instant,
    check: &mut dyn FnMut() -> Result<(), Unavailable>,
) -> Result<(), Unavailable> {
    #[cfg(test)]
    if let Some(result) = TEST_RELAY.with(|cell| {
        let mut borrow = cell.borrow_mut();
        borrow.as_mut().map(|relay| relay())
    }) {
        return result;
    }
    relay::relay(endpoint, deadline, check, 0, 1)
}

fn serve_linux(
    config: &Value,
    session: &Value,
    deadline: Instant,
    check: &mut dyn FnMut() -> Result<(), Unavailable>,
) -> Result<(), Unavailable> {
    // A socket pair gives inetd a genuine bidirectional socket, even though SSH
    // exposes separate pipes. x11vnc never binds a guest TCP port.
    let directory = tempfile::Builder::new()
        .prefix("vm-console-")
        .tempdir()
        .map_err(|_| Unavailable::new("stream_failed"))?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| Unavailable::new("stream_failed"))?;

    let password_path = directory.path().join("password");
    let password = config
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options
            .open(&password_path)
            .map_err(|_| Unavailable::new("stream_failed"))?;
        file.write_all(password.as_bytes())
            .map_err(|_| Unavailable::new("stream_failed"))?;
        file.write_all(b"\n")
            .map_err(|_| Unavailable::new("stream_failed"))?;
    }

    let (parent, child) = UnixStream::pair().map_err(|_| Unavailable::new("stream_failed"))?;
    let child_stdout = child
        .try_clone()
        .map_err(|_| Unavailable::new("stream_failed"))?;

    let argv = linux::x11vnc_argv(session, &password_path.to_string_lossy());
    let mut guard = ProcessGuard(None);

    // The session is re-checked before x11vnc is started, exactly as Python
    // does, so a changed session never spawns a process.
    check()?;

    guard.0 = Some(spawn_x11vnc(&argv, session, child, child_stdout)?);

    run_relay(&parent, deadline, &mut *check)?;
    Ok(())
}

/// Start x11vnc with stdin/stdout bound to the same socket, or record the
/// request through the test spawn seam and return a fake pid.
fn spawn_x11vnc(
    argv: &[String],
    session: &Value,
    child: UnixStream,
    child_stdout: UnixStream,
) -> Result<RunningChild, Unavailable> {
    let environment: Vec<(String, String)> = vec![
        (
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        ),
        ("LANG".to_string(), "C".to_string()),
        ("LC_ALL".to_string(), "C".to_string()),
        (
            "DISPLAY".to_string(),
            linux::field_string(session, "display"),
        ),
        (
            "XAUTHORITY".to_string(),
            linux::field_string(session, "xauthority"),
        ),
    ];

    // The stderr choice is written once and feeds both the seam request and the
    // real child, so the test asserts the stream production actually installs.
    #[cfg(test)]
    let stderr_choice = TestStdio::Null;
    #[cfg(test)]
    if let Some(pid) = run_test_spawner(argv, &environment, &child, &child_stdout, stderr_choice)? {
        return Ok(RunningChild::Fake(pid));
    }

    let child_stdin_stdio = Stdio::from(OwnedFd::from(child));
    let child_stdout_stdio = Stdio::from(OwnedFd::from(child_stdout));
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.stdin(child_stdin_stdio);
    command.stdout(child_stdout_stdio);
    #[cfg(test)]
    command.stderr(match stderr_choice {
        TestStdio::Null => Stdio::null(),
    });
    #[cfg(not(test))]
    command.stderr(Stdio::null());
    command.env_clear();
    for (key, value) in &environment {
        command.env(key, value);
    }
    // SAFETY: `pre_exec` runs between fork and exec; `setsid` is
    // async-signal-safe and mirrors Python's `start_new_session=True`.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map(RunningChild::Real)
        .map_err(|_| Unavailable::new("stream_failed"))
}

/// Consult the test spawner. `Ok(None)` means no seam is installed and the
/// real process must be started; `Err` is the fake spawn failure.
#[cfg(test)]
fn run_test_spawner(
    argv: &[String],
    environment: &[(String, String)],
    child: &UnixStream,
    child_stdout: &UnixStream,
    stderr_choice: TestStdio,
) -> Result<Option<libc::pid_t>, Unavailable> {
    TEST_SPAWNER.with(|cell| {
        let mut borrow = cell.borrow_mut();
        match borrow.as_mut() {
            Some(spawner) => {
                let request = TestSpawnRequest {
                    argv: argv.to_vec(),
                    env: environment.to_vec(),
                    stdin: child.as_raw_fd(),
                    stdout: child_stdout.as_raw_fd(),
                    stderr: stderr_choice,
                };
                match spawner(request) {
                    Ok(pid) => Ok(Some(pid)),
                    Err(()) => Err(Unavailable::new("stream_failed")),
                }
            }
            None => Ok(None),
        }
    })
}

/// CLI entry point. Returns the process exit code.
pub fn run_main(args: &[String]) -> i32 {
    let parsed = match (
        args.len(),
        args.first().and_then(|value| Kind::parse(value)),
        args.get(1).map(String::as_str),
    ) {
        (2, Some(kind), Some(operation @ ("probe" | "serve"))) => (kind, operation),
        _ => {
            eprintln!("guest-console: arguments_invalid");
            return 2;
        }
    };
    let (kind, operation) = parsed;

    if operation == "probe" {
        let record = probe(kind);
        let serialized = serde_json::to_string(&record).unwrap_or_else(|_| "null".to_string());
        let mut stdout = io::stdout();
        let _ = stdout.write_all(serialized.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
        return 0;
    }

    signal::install_signal_handlers();
    match serve(kind) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("guest-console: {}", failure_message(&error));
            1
        }
    }
}
