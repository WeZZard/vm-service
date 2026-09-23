//! Attempt-local console broker.
//!
//! Python source: `bin/console_worker.py`. Control frames and credentials
//! arrive on one private file descriptor. The broker owns the loopback
//! listener, the SSH child that carries guest RFB, and the local viewer
//! process. No credential is ever placed in an argument vector, a log, or an
//! error message: credentials travel inside the configuration frame and, for
//! the viewer, on the viewer's standard input.
//!
//! Framing. A guest agent reads exactly one JSON configuration line from the
//! SSH child's standard input and then reuses that same stdin/stdout for binary
//! RFB. The broker therefore prepends `guest_config` serialized compactly plus
//! `\n` to everything it writes to the guest, before any RFB byte, and it never
//! reads the guest RFB stream as its own input.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::ser::Formatter;
use serde_json::{json, Map, Value};

/// Largest control line, in bytes, excluding its terminating newline.
pub const MAX_COMMAND: usize = 65536;
/// Largest pending event output, in bytes.
pub const MAX_BUFFER: usize = 262144;
/// Largest granted lifetime, in seconds.
pub const MAX_GRANT: f64 = 720.0 * 3600.0;

/// The process-wide stop latch set from an asynchronous signal handler.
static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);

/// POSIX monotonic seconds, matching Python `time.monotonic()`.
///
/// Control frames carry deadlines measured with this clock, so the broker and
/// its controller must read the same quantity.
pub fn monotonic() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // `clock_gettime` cannot fail for CLOCK_MONOTONIC on a supported kernel.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// POSIX wall-clock seconds, matching Python `time.time()`.
pub fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Spawns a child and returns it, or an OS error.
///
/// `stdout_piped` mirrors the Python call: SSH children keep a standard-output
/// pipe for binary RFB; the viewer's standard output is discarded.
pub type Launcher = Arc<dyn Fn(&[String], bool) -> io::Result<Child> + Send + Sync>;

/// The production launcher: `stdin=PIPE`, `stderr=DEVNULL`, and a new session.
pub fn real_launch(argv: &[String], stdout_piped: bool) -> io::Result<Child> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::piped());
    if stdout_piped {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }
    cmd.stderr(Stdio::null());
    // Every launched child owns its process group; never target an existing app.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// The default launcher used by [`serve_control`].
pub fn default_launcher() -> Launcher {
    Arc::new(real_launch)
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn set_cloexec(fd: RawFd) {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

/// Kills a child's process group and reaps it within a 0.2 s bound.
///
/// Returns whether the child has exited, matching Python `terminate()`.
fn terminate(child: &mut Child) -> bool {
    kill_group(child.id());
    drop(child.stdin.take());
    drop(child.stdout.take());
    drop(child.stderr.take());
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return matches!(child.try_wait(), Ok(Some(_)));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
}

/// A file descriptor this process owns and closes exactly once.
struct OwnedFd(RawFd);

impl OwnedFd {
    fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn wipe(buf: &mut Vec<u8>) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    buf.clear();
}

fn recv_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn send_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::send(fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn is_would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

/// Python's default `json.dumps` separators, used only for the guest size bound.
struct PyJsonFormatter;

impl Formatter for PyJsonFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        writer.write_all(b": ")
    }
}

/// Byte length of Python `json.dumps(value)` with default separators.
///
/// Non-ASCII characters are not escaped, matching the workspace-wide JSON
/// deviation; every other separator matches Python's default.
fn py_json_len(value: &Value) -> usize {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyJsonFormatter);
        let _ = value.serialize(&mut ser);
    }
    buf.len()
}

fn json_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// Python `grant(value)`: a finite numeric deadline within the grant window.
fn grant_value(value: &Value) -> bool {
    let n = match value {
        Value::Number(n) => n.as_f64(),
        _ => return false,
    };
    let n = match n {
        Some(n) if n.is_finite() => n,
        _ => return false,
    };
    monotonic() < n && n <= monotonic() + MAX_GRANT
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ssh,
    Viewer,
}

struct LaunchResult {
    kind: Kind,
    conn: Option<usize>,
    process: Option<Child>,
    payload: Arc<Mutex<Vec<u8>>>,
}

struct PasswordState {
    fd: RawFd,
    buf: Arc<Mutex<Vec<u8>>>,
}

struct Connection {
    socket: Option<OwnedFd>,
    closed: bool,
    to_guest: Vec<u8>,
    to_viewer: Vec<u8>,
    banner: Vec<u8>,
    banner_valid: bool,
    output_eof: bool,
    input_closed: bool,
    process: Option<Child>,
}

impl Connection {
    fn new(fd: RawFd) -> Self {
        Connection {
            socket: Some(OwnedFd(fd)),
            closed: false,
            to_guest: Vec::new(),
            to_viewer: Vec::new(),
            banner: Vec::new(),
            banner_valid: false,
            output_eof: false,
            input_closed: false,
            process: None,
        }
    }

    fn socket_fd(&self) -> Option<RawFd> {
        self.socket.as_ref().map(|s| s.raw())
    }
}

/// State shared with the launch threads and with observers.
struct Shared {
    stopped: AtomicBool,
    pending: AtomicUsize,
    results: Mutex<VecDeque<LaunchResult>>,
}

/// The broker's authoritative state. It lives behind a mutex so a supervising
/// test can observe it while the loop blocks in `poll`.
pub struct State {
    control: Option<OwnedFd>,
    input: Vec<u8>,
    output: Vec<u8>,
    deadline: f64,
    generation: i64,
    config: Option<Arc<Mutex<Map<String, Value>>>>,
    listener: Option<OwnedFd>,
    opened: bool,
    reason: String,
    launch_threads: Vec<Option<JoinHandle<()>>>,
    launch_payloads: Vec<Arc<Mutex<Vec<u8>>>>,
    children: Vec<Child>,
    connections: Vec<Connection>,
    accepted: u64,
    viewer_index: Option<usize>,
    password: Option<PasswordState>,
    shared: Arc<Shared>,
}

impl State {
    /// Configured grant deadline, in monotonic seconds.
    pub fn deadline(&self) -> f64 {
        self.deadline
    }

    /// Latest accepted generation.
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// Number of launcher invocations whose result has not been collected.
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// Loopback listener address, or `None` once it is closed.
    pub fn listener_addr(&self) -> Option<SocketAddr> {
        let fd = self.listener.as_ref()?.raw();
        let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockname(
                fd,
                &mut addr as *mut libc::sockaddr_in as *mut libc::sockaddr,
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let port = u16::from_be(addr.sin_port);
        Some(SocketAddr::new(IpAddr::V4(ip), port))
    }

    /// Retained configuration, or `None` after scrubbing.
    pub fn config_arc(&self) -> Option<Arc<Mutex<Map<String, Value>>>> {
        self.config.clone()
    }

    /// A retained initial-payload buffer by index.
    pub fn launch_payload_arc(&self, index: usize) -> Option<Arc<Mutex<Vec<u8>>>> {
        self.launch_payloads.get(index).cloned()
    }

    /// A copy of a retained initial-payload buffer by index.
    pub fn launch_payload(&self, index: usize) -> Option<Vec<u8>> {
        self.launch_payloads
            .get(index)
            .map(|p| p.lock().unwrap().clone())
    }

    /// Count of launcher threads started so far.
    pub fn launch_thread_count(&self) -> usize {
        self.launch_threads.len()
    }

    /// Waits up to `timeout` for each launcher thread, returning false on timeout.
    pub fn join_launch_threads(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut ok = true;
        for slot in self.launch_threads.iter_mut() {
            if let Some(handle) = slot.take() {
                while !handle.is_finished() {
                    if Instant::now() >= deadline {
                        ok = false;
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                }
            }
        }
        ok
    }

    /// Whether every recorded child has been reaped.
    pub fn all_children_exited(&mut self) -> bool {
        self.children
            .iter_mut()
            .all(|c| matches!(c.try_wait(), Ok(Some(_))))
    }

    /// A copy of the pending control input buffer.
    pub fn input_bytes(&self) -> Vec<u8> {
        self.input.clone()
    }

    /// The current revocation reason.
    pub fn reason_string(&self) -> String {
        self.reason.clone()
    }
}

/// The attempt-local broker.
pub struct Worker {
    state: Arc<Mutex<State>>,
    shared: Arc<Shared>,
    launcher: Launcher,
}

impl Worker {
    /// Wraps an already-open control socket descriptor.
    pub fn new(control_fd: RawFd, launcher: Launcher) -> Self {
        set_cloexec(control_fd);
        set_nonblocking(control_fd);
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            results: Mutex::new(VecDeque::new()),
        });
        let state = Arc::new(Mutex::new(State {
            control: Some(OwnedFd(control_fd)),
            input: Vec::new(),
            output: Vec::new(),
            deadline: monotonic() + 10.0,
            generation: -1,
            config: None,
            listener: None,
            opened: false,
            reason: "controller_lost".to_string(),
            launch_threads: Vec::new(),
            launch_payloads: Vec::new(),
            children: Vec::new(),
            connections: Vec::new(),
            accepted: 0,
            viewer_index: None,
            password: None,
            shared: shared.clone(),
        }));
        Worker {
            state,
            shared,
            launcher,
        }
    }

    /// Constructs a broker with the production launcher.
    pub fn with_default_launcher(control_fd: RawFd) -> Self {
        Worker::new(control_fd, default_launcher())
    }

    /// A handle to the mutable state, for observation.
    pub fn state_arc(&self) -> Arc<Mutex<State>> {
        self.state.clone()
    }

    /// Runs until revocation, then returns the reason.
    pub fn run(&self) -> String {
        if self.event_loop().is_err() {
            self.stop("worker_failed");
        }
        self.cleanup();
        self.reason()
    }

    fn stop(&self, reason: &str) {
        if !self.shared.stopped.load(Ordering::SeqCst) {
            self.state.lock().unwrap().reason = reason.to_string();
            self.shared.stopped.store(true, Ordering::SeqCst);
        }
    }

    fn reason(&self) -> String {
        self.state.lock().unwrap().reason.clone()
    }

    fn event_loop(&self) -> io::Result<()> {
        while !self.shared.stopped.load(Ordering::SeqCst) {
            if SIGNAL_STOP.load(Ordering::SeqCst) {
                self.stop("signal");
                break;
            }
            {
                let st = self.state.lock().unwrap();
                if monotonic() >= st.deadline {
                    drop(st);
                    self.stop("expired");
                    break;
                }
            }
            self.collect();
            {
                let mut st = self.state.lock().unwrap();
                let exited = match st.viewer_index {
                    Some(index) => match st.children.get_mut(index) {
                        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                        None => false,
                    },
                    None => false,
                };
                if exited {
                    drop(st);
                    self.stop("viewer_exited");
                    break;
                }
            }
            let entries = {
                let st = self.state.lock().unwrap();
                build_poll(&st)
            };
            let timeout_ms = {
                let st = self.state.lock().unwrap();
                let seconds = (st.deadline - monotonic()).clamp(0.0, 0.02);
                (seconds * 1000.0).ceil() as libc::c_int
            };
            let mut fds: Vec<libc::pollfd> = entries.iter().map(|(p, _)| *p).collect();
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if rc == 0 {
                continue;
            }
            for (index, (_, target)) in entries.iter().enumerate() {
                let revents = fds[index].revents;
                if revents == 0 {
                    continue;
                }
                // Check authority again after waiting, before every dispatch.
                if SIGNAL_STOP.load(Ordering::SeqCst) {
                    self.stop("signal");
                }
                if self.shared.stopped.load(Ordering::SeqCst) {
                    break;
                }
                {
                    let st = self.state.lock().unwrap();
                    if monotonic() >= st.deadline {
                        drop(st);
                        self.stop("expired");
                    }
                }
                if self.shared.stopped.load(Ordering::SeqCst) {
                    break;
                }
                match self.dispatch(*target, revents) {
                    Ok(()) => {}
                    Err(CmdError::Invalid) => {
                        let mut st = self.state.lock().unwrap();
                        emit(
                            &mut st,
                            &self.shared,
                            "error",
                            &[("reason", json!("invalid_command"))],
                        );
                        stop_locked(&mut st, &self.shared, "invalid_command");
                    }
                    Err(CmdError::Io(err)) => return Err(err),
                }
            }
        }
        Ok(())
    }

    fn collect(&self) {
        loop {
            let result = { self.shared.results.lock().unwrap().pop_front() };
            let result = match result {
                Some(r) => r,
                None => break,
            };
            self.shared.pending.fetch_sub(1, Ordering::SeqCst);
            let mut st = self.state.lock().unwrap();
            let mut process = match result.process {
                Some(p) => p,
                None => {
                    emit(
                        &mut st,
                        &self.shared,
                        "error",
                        &[("reason", json!("launch_failed"))],
                    );
                    stop_locked(&mut st, &self.shared, "launch_failed");
                    continue;
                }
            };
            if let Some(stdin) = process.stdin.as_ref() {
                set_nonblocking(stdin.as_raw_fd());
            }
            if let Some(stdout) = process.stdout.as_ref() {
                set_nonblocking(stdout.as_raw_fd());
            }
            match result.kind {
                Kind::Viewer => {
                    let has_payload = !result.payload.lock().unwrap().is_empty();
                    let stdin_fd = process.stdin.as_ref().map(|s| s.as_raw_fd());
                    st.children.push(process);
                    let index = st.children.len() - 1;
                    st.viewer_index = Some(index);
                    if has_payload {
                        if let Some(fd) = stdin_fd {
                            st.password = Some(PasswordState {
                                fd,
                                buf: result.payload.clone(),
                            });
                        } else {
                            let _ = st.children[index].stdin.take();
                            emit(
                                &mut st,
                                &self.shared,
                                "launched",
                                &[("authentication", json!("required"))],
                            );
                        }
                    } else {
                        let _ = st.children[index].stdin.take();
                        emit(
                            &mut st,
                            &self.shared,
                            "launched",
                            &[("authentication", json!("required"))],
                        );
                    }
                }
                Kind::Ssh => {
                    let index = match result.conn {
                        Some(i) => i,
                        None => continue,
                    };
                    if index >= st.connections.len() || st.connections[index].closed {
                        terminate(&mut process);
                        continue;
                    }
                    st.connections[index].process = Some(process);
                    let mut payload = result.payload.lock().unwrap();
                    st.connections[index]
                        .to_guest
                        .splice(0..0, payload.iter().copied());
                    wipe(&mut payload);
                }
            }
        }
    }

    fn cleanup(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        let mut st = self.state.lock().unwrap();
        st.listener = None;
        for index in 0..st.connections.len() {
            close_connection_locked(&mut st, &self.shared, index);
        }
        emit(
            &mut st,
            &self.shared,
            "transport",
            &[("state", json!("revoked"))],
        );
        scrub(&mut st);
        {
            let mut queue = self.shared.results.lock().unwrap();
            while let Some(result) = queue.pop_front() {
                self.shared.pending.fetch_sub(1, Ordering::SeqCst);
                if let Some(process) = result.process {
                    st.children.push(process);
                }
            }
        }
        let mut cleaned = true;
        for child in st.children.iter_mut() {
            let stopped = terminate(child);
            cleaned = stopped && cleaned;
        }
        let pending = self.shared.pending.load(Ordering::SeqCst);
        let state_text = if pending > 0 || !cleaned {
            "pending"
        } else {
            "local_children_stopped"
        };
        let reason = st.reason.clone();
        emit(
            &mut st,
            &self.shared,
            "cleanup",
            &[("state", json!(state_text))],
        );
        emit(
            &mut st,
            &self.shared,
            "closed",
            &[("reason", json!(reason))],
        );
        // Best effort, never delay revocation.
        if !st.output.is_empty() {
            if let Some(fd) = st.control.as_ref().map(|c| c.raw()) {
                let _ = send_fd(fd, &st.output);
            }
        }
        st.control = None;
    }

    fn close_connection(&self, index: usize) {
        let mut st = self.state.lock().unwrap();
        close_connection_locked(&mut st, &self.shared, index);
    }

    fn dispatch(&self, target: Target, revents: libc::c_short) -> Result<(), CmdError> {
        const READ_MASK: libc::c_short = libc::POLLIN | libc::POLLHUP | libc::POLLERR;
        match target {
            Target::Control => {
                let fd = match self.state.lock().unwrap().control.as_ref() {
                    Some(c) => c.raw(),
                    None => return Ok(()),
                };
                if revents & READ_MASK != 0 {
                    let mut buf = [0u8; 4096];
                    let n = match recv_fd(fd, &mut buf) {
                        Ok(n) => n,
                        Err(e) if is_would_block(&e) => return Ok(()),
                        Err(e) => return Err(CmdError::Io(e)),
                    };
                    if n == 0 {
                        self.stop("controller_lost");
                        return Ok(());
                    }
                    self.state
                        .lock()
                        .unwrap()
                        .input
                        .extend_from_slice(&buf[..n]);
                    loop {
                        let mut st = self.state.lock().unwrap();
                        let position = match st.input.iter().position(|&b| b == b'\n') {
                            Some(p) => p,
                            None => break,
                        };
                        let line: Vec<u8> = st.input.drain(..=position).collect();
                        let line = &line[..line.len() - 1];
                        if line.len() > MAX_COMMAND {
                            return Err(CmdError::Invalid);
                        }
                        let value: Value = match serde_json::from_slice(line) {
                            Ok(v) => v,
                            Err(_) => return Err(CmdError::Invalid),
                        };
                        command(&mut st, &self.shared, &self.launcher, value)?;
                        if self.shared.stopped.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                    }
                    let st = self.state.lock().unwrap();
                    if st.input.len() > MAX_COMMAND {
                        return Err(CmdError::Invalid);
                    }
                }
                if revents & libc::POLLOUT != 0 {
                    let mut st = self.state.lock().unwrap();
                    if !st.output.is_empty() {
                        match send_fd(fd, &st.output) {
                            Ok(n) => {
                                st.output.drain(..n);
                            }
                            Err(e) if is_would_block(&e) => {}
                            Err(e) => return Err(CmdError::Io(e)),
                        }
                    }
                }
                Ok(())
            }
            Target::Listen => {
                let listener_fd = match self.state.lock().unwrap().listener.as_ref() {
                    Some(l) => l.raw(),
                    None => return Ok(()),
                };
                let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                let client_fd = unsafe {
                    libc::accept(
                        listener_fd,
                        &mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr,
                        &mut len,
                    )
                };
                if client_fd < 0 {
                    let err = io::Error::last_os_error();
                    if is_would_block(&err) {
                        return Ok(());
                    }
                    return Err(CmdError::Io(err));
                }
                set_nonblocking(client_fd);
                set_cloexec(client_fd);
                let mut st = self.state.lock().unwrap();
                let config = match st.config.as_ref() {
                    Some(c) => c.clone(),
                    None => {
                        unsafe {
                            libc::close(client_fd);
                        }
                        return Ok(());
                    }
                };
                let (limit, initial, argv) = {
                    let map = config.lock().unwrap();
                    let limit = map
                        .get("connection_limit")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(1);
                    let guest = map.get("guest_config").cloned().unwrap_or(Value::Null);
                    let mut initial = serde_json::to_vec(&guest).unwrap_or_default();
                    initial.push(b'\n');
                    let argv: Vec<String> = map
                        .get("ssh_argv")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    (limit, initial, argv)
                };
                if !st.opened || st.accepted as i64 >= limit {
                    unsafe {
                        libc::close(client_fd);
                    }
                    return Ok(());
                }
                st.accepted += 1;
                st.connections.push(Connection::new(client_fd));
                let index = st.connections.len() - 1;
                launch(
                    &mut st,
                    &self.shared,
                    &self.launcher,
                    Kind::Ssh,
                    Some(index),
                    argv,
                    initial,
                );
                Ok(())
            }
            Target::Password => {
                let (fd, buffer) = match self.state.lock().unwrap().password.as_ref() {
                    Some(p) => (p.fd, p.buf.clone()),
                    None => return Ok(()),
                };
                let mut buf = buffer.lock().unwrap();
                match write_fd(fd, &buf) {
                    Ok(n) => {
                        buf.drain(..n);
                        if buf.is_empty() {
                            drop(buf);
                            let mut st = self.state.lock().unwrap();
                            st.password = None;
                            if let Some(index) = st.viewer_index {
                                if let Some(child) = st.children.get_mut(index) {
                                    let _ = child.stdin.take();
                                }
                            }
                            emit(
                                &mut st,
                                &self.shared,
                                "launched",
                                &[("authentication", json!("password_supplied"))],
                            );
                        }
                    }
                    // BlockingIOError is an OSError here, so it takes this branch.
                    Err(_) => {
                        drop(buf);
                        let mut st = self.state.lock().unwrap();
                        st.password = None;
                        if let Some(index) = st.viewer_index {
                            if let Some(child) = st.children.get_mut(index) {
                                let _ = child.stdin.take();
                            }
                        }
                        stop_locked(&mut st, &self.shared, "viewer_failed");
                    }
                }
                Ok(())
            }
            Target::Socket(index) => {
                if revents & READ_MASK != 0 {
                    let mut st = self.state.lock().unwrap();
                    if index >= st.connections.len() || st.connections[index].closed {
                        return Ok(());
                    }
                    let capacity = (MAX_BUFFER - MAX_COMMAND)
                        .saturating_sub(st.connections[index].to_guest.len())
                        .clamp(1, 65536);
                    let fd = match st.connections[index].socket_fd() {
                        Some(fd) => fd,
                        None => return Ok(()),
                    };
                    let mut buf = vec![0u8; capacity];
                    match recv_fd(fd, &mut buf) {
                        Ok(0) => {
                            drop(st);
                            self.close_connection(index);
                            return Ok(());
                        }
                        Ok(n) => {
                            st.connections[index].to_guest.extend_from_slice(&buf[..n]);
                        }
                        Err(e) if is_would_block(&e) => return Ok(()),
                        Err(_) => {
                            drop(st);
                            self.close_connection(index);
                            return Ok(());
                        }
                    }
                }
                if revents & libc::POLLOUT != 0 {
                    let mut st = self.state.lock().unwrap();
                    if index >= st.connections.len() || st.connections[index].closed {
                        return Ok(());
                    }
                    let fd = match st.connections[index].socket_fd() {
                        Some(fd) => fd,
                        None => return Ok(()),
                    };
                    let out = std::mem::take(&mut st.connections[index].to_viewer);
                    match send_fd(fd, &out) {
                        Ok(n) => {
                            st.connections[index].to_viewer = out[n..].to_vec();
                        }
                        Err(e) if is_would_block(&e) => {
                            st.connections[index].to_viewer = out;
                            return Ok(());
                        }
                        Err(_) => {
                            drop(st);
                            self.close_connection(index);
                            return Ok(());
                        }
                    }
                    if st.connections[index].output_eof
                        && st.connections[index].to_viewer.is_empty()
                    {
                        drop(st);
                        self.close_connection(index);
                        return Ok(());
                    }
                }
                Ok(())
            }
            Target::Input(index) => {
                let mut st = self.state.lock().unwrap();
                if index >= st.connections.len() || st.connections[index].closed {
                    return Ok(());
                }
                let fd = match st.connections[index]
                    .process
                    .as_ref()
                    .and_then(|p| p.stdin.as_ref())
                {
                    Some(stdin) => stdin.as_raw_fd(),
                    None => return Ok(()),
                };
                let mut data = std::mem::take(&mut st.connections[index].to_guest);
                match write_fd(fd, &data) {
                    Ok(n) => {
                        st.connections[index].to_guest = data[n..].to_vec();
                    }
                    Err(e) if is_would_block(&e) => {
                        st.connections[index].to_guest = data;
                        return Ok(());
                    }
                    Err(_) => {
                        // A child may close stdin before its final stdout drains.
                        wipe(&mut data);
                        st.connections[index].input_closed = true;
                        wipe(&mut st.connections[index].to_guest);
                    }
                }
                Ok(())
            }
            Target::Output(index) => {
                let mut st = self.state.lock().unwrap();
                if index >= st.connections.len() || st.connections[index].closed {
                    return Ok(());
                }
                let banner_valid = st.connections[index].banner_valid;
                let capacity = if banner_valid {
                    MAX_BUFFER.saturating_sub(st.connections[index].to_viewer.len())
                } else {
                    12usize.saturating_sub(st.connections[index].banner.len())
                };
                let size = capacity.clamp(1, 65536);
                let fd = match st.connections[index]
                    .process
                    .as_ref()
                    .and_then(|p| p.stdout.as_ref())
                {
                    Some(stdout) => stdout.as_raw_fd(),
                    None => return Ok(()),
                };
                let mut buf = vec![0u8; size];
                match read_fd(fd, &mut buf) {
                    Ok(0) => {
                        st.connections[index].output_eof = true;
                        if st.connections[index].to_viewer.is_empty() {
                            drop(st);
                            self.close_connection(index);
                        }
                    }
                    Ok(n) => {
                        if !st.connections[index].banner_valid {
                            st.connections[index].banner.extend_from_slice(&buf[..n]);
                            if st.connections[index].banner.len() == 12 {
                                let apple = match st.config.as_ref() {
                                    Some(cfg) => {
                                        let map = cfg.lock().unwrap();
                                        map.get("viewer")
                                            .and_then(|v| v.get("kind"))
                                            .and_then(|k| k.as_str())
                                            == Some("apple")
                                    }
                                    None => false,
                                };
                                let banner = st.connections[index].banner.clone();
                                let supported = banner.as_slice() == b"RFB 003.003\n".as_slice()
                                    || banner.as_slice() == b"RFB 003.007\n".as_slice()
                                    || banner.as_slice() == b"RFB 003.008\n".as_slice()
                                    || (apple && banner.as_slice() == b"RFB 003.889\n".as_slice());
                                if !supported {
                                    drop(st);
                                    self.close_connection(index);
                                    return Ok(());
                                }
                                st.connections[index].banner_valid = true;
                                let banner = std::mem::take(&mut st.connections[index].banner);
                                st.connections[index].to_viewer.extend_from_slice(&banner);
                                emit(
                                    &mut st,
                                    &self.shared,
                                    "transport",
                                    &[("state", json!("connected"))],
                                );
                            }
                        } else {
                            st.connections[index].to_viewer.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(e) if is_would_block(&e) => return Ok(()),
                    Err(_) => {
                        drop(st);
                        self.close_connection(index);
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Target {
    Control,
    Listen,
    Socket(usize),
    Input(usize),
    Output(usize),
    Password,
}

enum CmdError {
    Invalid,
    Io(io::Error),
}

fn close_connection_locked(st: &mut State, shared: &Shared, index: usize) {
    if index >= st.connections.len() {
        return;
    }
    if st.connections[index].closed {
        return;
    }
    st.connections[index].closed = true;
    st.connections[index].socket = None;
    if let Some(process) = st.connections[index].process.as_mut() {
        let _ = process.stdin.take();
        let _ = process.stdout.take();
    }
    if let Some(process) = st.connections[index].process.as_ref() {
        kill_group(process.id());
    }
    emit(st, shared, "transport", &[("state", json!("closed"))]);
}

fn scrub(st: &mut State) {
    wipe(&mut st.input);
    if let Some(config) = st.config.take() {
        let mut map = config.lock().unwrap();
        for name in ["guest_config", "viewer"] {
            if let Some(Value::Object(obj)) = map.get_mut(name) {
                obj.remove("password");
            }
        }
    }
    for payload in st.launch_payloads.iter() {
        wipe(&mut payload.lock().unwrap());
    }
    for connection in st.connections.iter_mut() {
        wipe(&mut connection.to_guest);
        wipe(&mut connection.to_viewer);
    }
    if let Some(password) = st.password.as_ref() {
        wipe(&mut password.buf.lock().unwrap());
    }
}

fn emit(st: &mut State, shared: &Shared, event: &str, fields: &[(&str, Value)]) {
    let mut map = Map::new();
    map.insert("op".to_string(), Value::String(event.to_string()));
    for (key, value) in fields {
        map.insert((*key).to_string(), value.clone());
    }
    if event == "transport" {
        let connected = map.get("state").and_then(|v| v.as_str()) == Some("connected");
        map.insert("connected".to_string(), Value::Bool(connected));
    }
    let mut data = serde_json::to_vec(&Value::Object(map)).unwrap_or_default();
    data.push(b'\n');
    if st.output.len() + data.len() > MAX_BUFFER {
        stop_locked(st, shared, "event_overflow");
        return;
    }
    st.output.extend_from_slice(&data);
}

fn stop_locked(st: &mut State, shared: &Shared, reason: &str) {
    if !shared.stopped.swap(true, Ordering::SeqCst) {
        st.reason = reason.to_string();
    }
}

fn launch_process(launcher: &Launcher, kind: Kind, argv: &[String]) -> io::Result<Child> {
    let stdout_piped = kind == Kind::Ssh;
    match launcher(argv, stdout_piped) {
        Ok(child) => Ok(child),
        Err(err) if kind == Kind::Viewer && err.raw_os_error() == Some(libc::ENOEXEC) => {
            // Installed TurboVNC wrappers may intentionally lack a shebang.
            let mut shell: Vec<String> = Vec::with_capacity(argv.len() + 1);
            shell.push("/bin/sh".to_string());
            shell.extend(argv.iter().cloned());
            launcher(&shell, false)
        }
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
fn launch(
    st: &mut State,
    shared: &Arc<Shared>,
    launcher: &Launcher,
    kind: Kind,
    conn: Option<usize>,
    argv: Vec<String>,
    initial: Vec<u8>,
) {
    let payload = Arc::new(Mutex::new(initial));
    st.launch_payloads.push(payload.clone());
    shared.pending.fetch_add(1, Ordering::SeqCst);
    let shared = shared.clone();
    let launcher = launcher.clone();
    let payload_handle = payload.clone();
    let handle = thread::spawn(move || {
        let mut process = launch_process(&launcher, kind, &argv).ok();
        if !shared.stopped.load(Ordering::SeqCst) {
            shared.results.lock().unwrap().push_back(LaunchResult {
                kind,
                conn,
                process: process.take(),
                payload: payload_handle.clone(),
            });
            return;
        }
        if let Some(child) = process.as_mut() {
            while !terminate(child) {
                thread::sleep(Duration::from_millis(50));
            }
        }
        shared.pending.fetch_sub(1, Ordering::SeqCst);
        wipe(&mut payload_handle.lock().unwrap());
    });
    st.launch_threads.push(Some(handle));
}

fn build_poll(st: &State) -> Vec<(libc::pollfd, Target)> {
    let mut entries: Vec<(libc::pollfd, Target)> = Vec::new();
    if let Some(control) = st.control.as_ref() {
        let mut events: libc::c_short = libc::POLLIN;
        if !st.output.is_empty() {
            events |= libc::POLLOUT;
        }
        entries.push((mkpoll(control.raw(), events), Target::Control));
    }
    if let Some(listener) = st.listener.as_ref() {
        entries.push((mkpoll(listener.raw(), libc::POLLIN), Target::Listen));
    }
    for (index, connection) in st.connections.iter().enumerate() {
        if connection.closed {
            continue;
        }
        let mut events: libc::c_short = 0;
        if !connection.output_eof
            && !connection.input_closed
            && connection.to_guest.len() < MAX_BUFFER - MAX_COMMAND
        {
            events |= libc::POLLIN;
        }
        if !connection.to_viewer.is_empty() {
            events |= libc::POLLOUT;
        }
        if events != 0 {
            if let Some(fd) = connection.socket_fd() {
                entries.push((mkpoll(fd, events), Target::Socket(index)));
            }
        }
        if let Some(process) = connection.process.as_ref() {
            if let Some(stdin) = process.stdin.as_ref() {
                let mut input_events: libc::c_short = 0;
                if !connection.output_eof
                    && !connection.input_closed
                    && !connection.to_guest.is_empty()
                {
                    input_events |= libc::POLLOUT;
                }
                if input_events != 0 {
                    entries.push((
                        mkpoll(stdin.as_raw_fd(), input_events),
                        Target::Input(index),
                    ));
                }
            }
            if let Some(stdout) = process.stdout.as_ref() {
                let mut output_events: libc::c_short = 0;
                if !connection.output_eof && connection.to_viewer.len() < MAX_BUFFER {
                    output_events |= libc::POLLIN;
                }
                if output_events != 0 {
                    entries.push((
                        mkpoll(stdout.as_raw_fd(), output_events),
                        Target::Output(index),
                    ));
                }
            }
        }
    }
    if let Some(password) = st.password.as_ref() {
        entries.push((mkpoll(password.fd, libc::POLLOUT), Target::Password));
    }
    entries
}

fn mkpoll(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn command(
    st: &mut State,
    shared: &Arc<Shared>,
    launcher: &Launcher,
    value: Value,
) -> Result<(), CmdError> {
    let obj = match value {
        Value::Object(map) => map,
        _ => return Err(CmdError::Invalid),
    };
    let op = match obj.get("op") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(CmdError::Invalid),
    };
    let (required, optional): (&[&str], &[&str]) = match op.as_str() {
        "configure" => (
            &[
                "op",
                "deadline",
                "generation",
                "ssh_argv",
                "guest_config",
                "viewer",
            ],
            &["connection_limit"],
        ),
        "renew" => (&["op", "deadline", "generation"], &[]),
        "open" => (&["op"], &[]),
        "cancel" => (&["op"], &[]),
        _ => return Err(CmdError::Invalid),
    };
    if required.iter().any(|key| !obj.contains_key(*key)) {
        return Err(CmdError::Invalid);
    }
    if obj
        .keys()
        .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(CmdError::Invalid);
    }
    if op == "cancel" {
        stop_locked(st, shared, "cancelled");
        return Ok(());
    }
    if op == "configure" && st.config.is_none() {
        return configure(st, shared, &obj);
    }
    if op == "renew" && st.config.is_some() {
        return renew(st, shared, &obj);
    }
    if op == "open" && st.config.is_some() && !st.opened {
        return open(st, shared, launcher, &obj);
    }
    Err(CmdError::Invalid)
}

fn configure(st: &mut State, shared: &Shared, obj: &Map<String, Value>) -> Result<(), CmdError> {
    let generation = match obj.get("generation") {
        Some(v) => json_int(v).ok_or(CmdError::Invalid)?,
        None => return Err(CmdError::Invalid),
    };
    if generation < 0 {
        return Err(CmdError::Invalid);
    }
    let deadline = match obj.get("deadline") {
        Some(v) => v,
        None => return Err(CmdError::Invalid),
    };
    if !grant_value(deadline) {
        return Err(CmdError::Invalid);
    }
    let deadline_seconds = match deadline {
        Value::Number(n) => n.as_f64().ok_or(CmdError::Invalid)?,
        _ => return Err(CmdError::Invalid),
    };
    let argv_values = match obj.get("ssh_argv") {
        Some(Value::Array(a)) => a,
        _ => return Err(CmdError::Invalid),
    };
    if argv_values.is_empty() || argv_values.len() > 128 {
        return Err(CmdError::Invalid);
    }
    let mut argv: Vec<String> = Vec::with_capacity(argv_values.len());
    for value in argv_values {
        match value {
            Value::String(s) if !s.contains('\0') => argv.push(s.clone()),
            _ => return Err(CmdError::Invalid),
        }
    }
    if !argv[0].starts_with('/') {
        return Err(CmdError::Invalid);
    }
    let guest = match obj.get("guest_config") {
        Some(Value::Object(map)) => map,
        _ => return Err(CmdError::Invalid),
    };
    let viewer = match obj.get("viewer") {
        Some(Value::Object(map)) => map,
        _ => return Err(CmdError::Invalid),
    };

    for key in ["version", "session", "expires_at"] {
        if !guest.contains_key(key) {
            return Err(CmdError::Invalid);
        }
    }
    if guest
        .keys()
        .any(|key| !["version", "session", "expires_at", "password"].contains(&key.as_str()))
    {
        return Err(CmdError::Invalid);
    }
    let version = match guest.get("version") {
        Some(v) => json_int(v).ok_or(CmdError::Invalid)?,
        None => return Err(CmdError::Invalid),
    };
    if version != 1 {
        return Err(CmdError::Invalid);
    }
    let session = match guest.get("session") {
        Some(Value::Object(map)) => map,
        _ => return Err(CmdError::Invalid),
    };
    match session.get("id") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => return Err(CmdError::Invalid),
    }
    let uid = match session.get("uid") {
        Some(v) => json_int(v).ok_or(CmdError::Invalid)?,
        None => return Err(CmdError::Invalid),
    };
    if uid <= 0 {
        return Err(CmdError::Invalid);
    }
    match session.get("user") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => return Err(CmdError::Invalid),
    }
    let expiry = match guest.get("expires_at") {
        Some(v) => v,
        None => return Err(CmdError::Invalid),
    };
    let expiry = match expiry {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .ok_or(CmdError::Invalid)?;
    if !expiry.is_finite() {
        return Err(CmdError::Invalid);
    }
    let remaining = expiry - wall_time();
    if !(remaining > 0.0 && remaining <= MAX_GRANT) {
        return Err(CmdError::Invalid);
    }
    if py_json_len(&Value::Object(guest.clone())) > 16384 {
        return Err(CmdError::Invalid);
    }

    for key in ["kind", "executable"] {
        if !viewer.contains_key(key) {
            return Err(CmdError::Invalid);
        }
    }
    if viewer
        .keys()
        .any(|key| !["kind", "executable", "password"].contains(&key.as_str()))
    {
        return Err(CmdError::Invalid);
    }
    let viewer_kind = match viewer.get("kind") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(CmdError::Invalid),
    };
    if viewer_kind != "apple" && viewer_kind != "turbovnc" {
        return Err(CmdError::Invalid);
    }
    let executable = match viewer.get("executable") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(CmdError::Invalid),
    };
    if !executable.starts_with('/') || executable.contains('\0') {
        return Err(CmdError::Invalid);
    }

    if let Some(password) = guest.get("password") {
        let password = match password {
            Value::String(s) => s.as_str(),
            _ => return Err(CmdError::Invalid),
        };
        if password.chars().count() != 8 {
            return Err(CmdError::Invalid);
        }
        if password.chars().any(|c| !('!'..='~').contains(&c)) {
            return Err(CmdError::Invalid);
        }
        if password.starts_with('#') {
            return Err(CmdError::Invalid);
        }
        if password.contains("__SKIP__") || password.contains("__COMM__") {
            return Err(CmdError::Invalid);
        }
    }
    let viewer_password: Option<&str> = match viewer.get("password") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(CmdError::Invalid),
        None => guest.get("password").and_then(|v| v.as_str()),
    };
    if let Some(password) = viewer_password {
        if password
            .chars()
            .any(|c| c == '\0' || c == '\r' || c == '\n')
        {
            return Err(CmdError::Invalid);
        }
    }
    if viewer_kind == "turbovnc" && viewer_password.is_none_or(|p| p.is_empty()) {
        return Err(CmdError::Invalid);
    }

    let limit = match obj.get("connection_limit") {
        None => 1i64,
        Some(v) => json_int(v).ok_or(CmdError::Invalid)?,
    };
    if !(1..=8).contains(&limit) {
        return Err(CmdError::Invalid);
    }

    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(CmdError::Io(io::Error::last_os_error()));
    }
    let listener = OwnedFd(fd);
    set_cloexec(fd);
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = 0;
    addr.sin_addr.s_addr = u32::from(Ipv4Addr::LOCALHOST).to_be();
    let bind_rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if bind_rc < 0 {
        return Err(CmdError::Io(io::Error::last_os_error()));
    }
    if unsafe { libc::listen(fd, 8) } < 0 {
        return Err(CmdError::Io(io::Error::last_os_error()));
    }
    set_nonblocking(fd);

    st.config = Some(Arc::new(Mutex::new(obj.clone())));
    st.generation = generation;
    st.deadline = deadline_seconds;
    st.listener = Some(listener);
    emit(st, shared, "ready", &[]);
    Ok(())
}

fn renew(st: &mut State, shared: &Shared, obj: &Map<String, Value>) -> Result<(), CmdError> {
    let generation = match obj.get("generation") {
        Some(v) => json_int(v).ok_or(CmdError::Invalid)?,
        None => return Err(CmdError::Invalid),
    };
    let deadline = match obj.get("deadline") {
        Some(v) => v,
        None => return Err(CmdError::Invalid),
    };
    if !grant_value(deadline) {
        return Err(CmdError::Invalid);
    }
    if generation <= st.generation {
        emit(
            st,
            shared,
            "error",
            &[("reason", json!("stale_generation"))],
        );
        return Ok(());
    }
    let deadline_seconds = match deadline {
        Value::Number(n) => n.as_f64().ok_or(CmdError::Invalid)?,
        _ => return Err(CmdError::Invalid),
    };
    st.generation = generation;
    st.deadline = deadline_seconds;
    emit(st, shared, "renewed", &[("generation", json!(generation))]);
    Ok(())
}

fn open(
    st: &mut State,
    shared: &Arc<Shared>,
    launcher: &Launcher,
    _obj: &Map<String, Value>,
) -> Result<(), CmdError> {
    let config = match st.config.as_ref() {
        Some(c) => c.clone(),
        None => return Err(CmdError::Invalid),
    };
    let (viewer_kind, executable, viewer_password) = {
        let map = config.lock().unwrap();
        let viewer = match map.get("viewer") {
            Some(v) => v,
            None => return Err(CmdError::Invalid),
        };
        let kind = viewer
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let executable = viewer
            .get("executable")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let password = match viewer.get("password").and_then(|v| v.as_str()) {
            Some(p) => Some(p.to_string()),
            None => map
                .get("guest_config")
                .and_then(|g| g.get("password"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };
        (kind, executable, password)
    };
    let port = st.listener_addr().map(|a| a.port()).unwrap_or(0);
    let (argv, password) = if viewer_kind == "apple" {
        (
            vec![
                "/usr/bin/open".to_string(),
                "-n".to_string(),
                "-W".to_string(),
                "-a".to_string(),
                executable,
                format!("vnc://127.0.0.1:{}", port),
            ],
            Vec::new(),
        )
    } else {
        (
            vec![
                executable,
                "-AutoPass".to_string(),
                "-ViewOnly".to_string(),
                "-NoReconnect".to_string(),
                format!("127.0.0.1::{}", port),
            ],
            format!("{}\n", viewer_password.unwrap_or_default()).into_bytes(),
        )
    };
    st.opened = true;
    launch(st, shared, launcher, Kind::Viewer, None, argv, password);
    Ok(())
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    SIGNAL_STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(
                signal,
                handle_signal as *const () as usize as libc::sighandler_t,
            );
        }
    }
}

/// Runs a broker over an inherited control descriptor until revocation.
///
/// This is the body of the `console-worker` executable and the entry point the
/// unit tests drive directly.
pub fn serve_control(control_fd: RawFd, launcher: Launcher) -> String {
    let worker = Worker::new(control_fd, launcher);
    install_signal_handlers();
    let reason = worker.run();
    // No access streams remain. Launcher threads independently clean late results.
    loop {
        let handle = {
            let mut st = worker.state.lock().unwrap();
            let mut taken = None;
            for slot in st.launch_threads.iter_mut() {
                if let Some(handle) = slot.take() {
                    taken = Some(handle);
                    break;
                }
            }
            taken
        };
        match handle {
            Some(handle) => {
                let _ = handle.join();
            }
            None => break,
        }
    }
    loop {
        let mut st = worker.state.lock().unwrap();
        let mut alive = false;
        for child in st.children.iter_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                alive = true;
                terminate(child);
            }
        }
        drop(st);
        if !alive {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    reason
}

/// Parses `--control-fd` and serves until revocation. Returns the exit code.
///
/// The Python `main()` accepts exactly `--control-fd`; the workspace
/// `CONVENTIONS.md` also names a credential descriptor, which the source does
/// not define. This matches the source: one descriptor carries control,
/// configuration, and credentials together.
pub fn main_impl() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let mut control_fd: Option<i32> = None;
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--control-fd" {
            index += 1;
            if index >= args.len() {
                eprintln!("console-worker: error: argument --control-fd: expected one argument");
                return 2;
            }
            match args[index].parse::<i32>() {
                Ok(value) => control_fd = Some(value),
                Err(_) => {
                    eprintln!(
                        "console-worker: error: argument --control-fd: invalid int value: '{}'",
                        args[index]
                    );
                    return 2;
                }
            }
        } else if let Some(value) = arg.strip_prefix("--control-fd=") {
            match value.parse::<i32>() {
                Ok(parsed) => control_fd = Some(parsed),
                Err(_) => {
                    eprintln!(
                        "console-worker: error: argument --control-fd: invalid int value: '{}'",
                        value
                    );
                    return 2;
                }
            }
        } else if arg == "-h" || arg == "--help" {
            println!("usage: console-worker --control-fd CONTROL_FD");
            return 0;
        } else {
            eprintln!("console-worker: error: unrecognized arguments: {}", arg);
            return 2;
        }
        index += 1;
    }
    let control_fd = match control_fd {
        Some(fd) => fd,
        None => {
            eprintln!("console-worker: error: the following arguments are required: --control-fd");
            return 2;
        }
    };
    serve_control(control_fd, default_launcher());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_launcher(calls: Arc<Mutex<Vec<Vec<String>>>>) -> Launcher {
        Arc::new(move |argv: &[String], _stdout_piped: bool| {
            calls.lock().unwrap().push(argv.to_vec());
            Err(io::Error::from_raw_os_error(libc::ENOEXEC))
        })
    }

    #[test]
    fn enoexec_fallback_retries_with_shell() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let launcher = recording_launcher(calls.clone());
        let argv = vec!["/opt/viewer/wrapper".to_string(), "-AutoPass".to_string()];
        let _ = launch_process(&launcher, Kind::Viewer, &argv);
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0], argv);
        assert_eq!(recorded[1][0], "/bin/sh");
        assert_eq!(&recorded[1][1..], &argv[..]);
    }

    #[test]
    fn non_viewer_enoexec_is_not_retried() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let launcher = recording_launcher(calls.clone());
        let argv = vec!["/usr/bin/ssh".to_string()];
        let _ = launch_process(&launcher, Kind::Ssh, &argv);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}
