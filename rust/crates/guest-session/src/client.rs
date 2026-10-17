//! The host side of a guest session (`docs/arch/guest-session.md`, GS-1 and
//! GS-5).
//!
//! A [`Session`] sends one request at a time and reports every failure of the
//! link as [`SessionError::Lost`], saying whether the request had reached its
//! commit point. [`SessionError::may_resend`] applies the GS-5 table: a
//! request that never reached its commit point had no effect and may be sent
//! again; a read may always be sent again; an exec or a write past its commit
//! point is uncertain and must not be.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::agent::{hex, CHUNK};
use crate::frame::{read_frame, write_frame, Frame, Kind};

/// Time the agent gets after a command's timeout to report its exit.
pub const EXIT_GRACE: Duration = Duration::from_secs(30);
/// Time the agent gets to report the exit of a cancelled command.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// How often a waiting request checks its cancellation flag.
const TICK: Duration = Duration::from_millis(50);
/// Alive frames the host may miss before it takes the link for dead (GS-1).
pub const MISSED_HEARTBEATS: u32 = 5;

/// The kinds of request, for the GS-5 resend rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Run a command.
    Exec,
    /// Write a file into the guest.
    Write,
    /// Read a file from the guest.
    Read,
}

/// How a request failed.
#[derive(Debug, Error)]
pub enum SessionError {
    /// The agent answered the request with an error. The session is intact.
    #[error("guest session agent error {code}: {message}")]
    Remote {
        /// A fixed code chosen by the agent, such as `not_found`.
        code: String,
        /// A human-readable explanation.
        message: String,
    },
    /// The link failed. `committed` says whether the request had reached its
    /// commit point, after which it may have taken effect in the guest.
    #[error("guest session lost ({}): {reason}", if *.committed { "after the commit point" } else { "before the commit point" })]
    Lost {
        /// Whether the request may have taken effect.
        committed: bool,
        /// What failed.
        reason: String,
    },
    /// The session's cancellation flag was set. The session is closed, so the
    /// agent stops what it started; `committed` is as for [`SessionError::Lost`].
    #[error("guest session cancelled ({})", if *.committed { "after the commit point" } else { "before the commit point" })]
    Cancelled {
        /// Whether the request may have taken effect.
        committed: bool,
    },
    /// A host-side file could not be read or written. The session is intact.
    #[error("host file error: {0}")]
    Local(String),
}

impl SessionError {
    /// Whether the request may be sent again on a new session (GS-5).
    pub fn may_resend(&self, op: Op) -> bool {
        match self {
            SessionError::Lost { committed, .. } => !committed || op == Op::Read,
            SessionError::Remote { .. }
            | SessionError::Local(_)
            | SessionError::Cancelled { .. } => false,
        }
    }

    /// Whether the outcome of the request is unknown (GS-5).
    pub fn is_uncertain(&self, op: Op) -> bool {
        matches!(
            self,
            SessionError::Lost {
                committed: true,
                ..
            } | SessionError::Cancelled { committed: true }
        ) && op != Op::Read
    }
}

/// The agent's hello frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The agent's protocol version.
    pub protocol: u64,
    /// The SHA-256 of the agent's executable.
    pub build: String,
    /// The guest's operating system, as Rust names it.
    pub os: String,
    /// The interval of the agent's alive frames, if it sends them.
    pub heartbeat: Option<Duration>,
}

/// The result of an exec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutcome {
    /// The exit code, or `None` when a signal ended the command.
    pub rc: Option<i32>,
    /// The signal that ended the command, if any.
    pub signal: Option<i32>,
    /// Whether the agent killed the command at its timeout.
    pub timed_out: bool,
    /// Whether the agent killed the command on a cancel frame.
    pub cancelled: bool,
    /// Combined standard output and error.
    pub output: Vec<u8>,
}

/// A completed transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// Bytes transferred.
    pub size: u64,
    /// SHA-256 of the bytes, checked by both ends.
    pub sha256: String,
}

enum Incoming {
    Frame(Frame),
    End(String),
}

/// One open guest session.
pub struct Session {
    writer: Box<dyn Write + Send>,
    frames: mpsc::Receiver<Incoming>,
    next: u64,
    broken: Option<String>,
    hello: Hello,
    last_used: Instant,
    cancel: Option<Arc<AtomicBool>>,
    /// When the last byte from the agent arrived.
    heard: Arc<Mutex<Instant>>,
    /// Set when the session is dropped, to stop its watchdog.
    closed: Arc<AtomicBool>,
}

impl Session {
    /// Start a session on a byte stream and wait up to `timeout` for the
    /// agent's hello.
    pub fn open(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        timeout: Duration,
    ) -> Result<Session, SessionError> {
        let (sender, frames) = mpsc::channel();
        let heard = Arc::new(Mutex::new(Instant::now()));
        let mut reader = Heard {
            inner: reader,
            heard: Arc::clone(&heard),
        };
        thread::spawn(move || loop {
            match read_frame(&mut reader) {
                Ok(Some(frame)) => {
                    if frame.kind == Kind::Alive {
                        continue;
                    }
                    if sender.send(Incoming::Frame(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(Incoming::End("the agent closed the session".into()));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(Incoming::End(error.to_string()));
                    return;
                }
            }
        });
        let lost = |reason: String| SessionError::Lost {
            committed: false,
            reason,
        };
        let hello = match frames.recv_timeout(timeout) {
            Ok(Incoming::Frame(frame)) if frame.kind == Kind::Hello => {
                let value = frame.value().map_err(|error| lost(error.to_string()))?;
                Hello {
                    protocol: value.get("protocol").and_then(Value::as_u64).unwrap_or(0),
                    build: text(&value, "build"),
                    os: text(&value, "os"),
                    heartbeat: value
                        .get("heartbeat_ms")
                        .and_then(Value::as_u64)
                        .filter(|&ms| ms > 0)
                        .map(Duration::from_millis),
                }
            }
            Ok(Incoming::Frame(frame)) => {
                return Err(lost(format!("expected hello, got {:?}", frame.kind)))
            }
            Ok(Incoming::End(reason)) => return Err(lost(reason)),
            Err(_) => return Err(lost(format!("no hello within {timeout:?}"))),
        };
        Ok(Session {
            writer: Box::new(writer),
            frames,
            next: 1,
            broken: None,
            hello,
            last_used: Instant::now(),
            cancel: None,
            heard,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The silence after which the link is taken for dead: the agent's
    /// heartbeat times [`MISSED_HEARTBEATS`], or never for an agent that
    /// sends no alive frames.
    pub fn silence_limit(&self) -> Option<Duration> {
        self.hello
            .heartbeat
            .map(|interval| interval * MISSED_HEARTBEATS)
    }

    /// Call `hook` once, from another thread, when the agent has been silent
    /// past [`Session::silence_limit`] while the session is open. The hook
    /// ends the channel, which also frees a request blocked in a write.
    pub fn on_silence(&mut self, hook: impl FnOnce() + Send + 'static) {
        let Some(limit) = self.silence_limit() else {
            return;
        };
        let heard = Arc::clone(&self.heard);
        let closed = Arc::clone(&self.closed);
        let check = (limit / 4).min(Duration::from_millis(500));
        thread::spawn(move || loop {
            thread::sleep(check);
            if closed.load(Ordering::SeqCst) {
                return;
            }
            if heard.lock().unwrap_or_else(|p| p.into_inner()).elapsed() > limit {
                hook();
                return;
            }
        });
    }

    /// Why the link counts as dead, if the agent has been silent too long.
    fn silent(&self) -> Option<String> {
        let limit = self.silence_limit()?;
        let quiet = self
            .heard
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed();
        (quiet > limit).then(|| format!("nothing from the agent for {:.1} s", quiet.as_secs_f64()))
    }

    /// Watch `flag` from now on. Once it is set, a running exec is cancelled
    /// in the guest, any other request closes the session, and no new request
    /// is sent.
    pub fn cancel_on(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }

    /// The agent's hello.
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Why the session can no longer be used, if it cannot.
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// Time since the session last finished a request.
    pub fn idle(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Round-trip a ping.
    pub fn ping(&mut self, timeout: Duration) -> Result<Duration, SessionError> {
        let started = Instant::now();
        let id = self.begin()?;
        self.send(
            &Frame::new(Kind::Ping, id, id.to_be_bytes().to_vec()),
            false,
        )?;
        let deadline = started + timeout;
        loop {
            let frame = self.wait(id, deadline, true)?;
            if frame.kind == Kind::Pong && frame.payload == id.to_be_bytes() {
                self.last_used = Instant::now();
                return Ok(started.elapsed());
            }
        }
    }

    /// Run `command` through the guest user's shell, as `ssh` does. With
    /// `stdin`, the bytes become the command's standard input. When the
    /// session's cancellation flag is set, the agent kills the command.
    pub fn exec(
        &mut self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutcome, SessionError> {
        let id = self.begin()?;
        let request = json!({
            "command": command,
            "stdin": stdin.is_some(),
            "timeout_s": timeout.as_secs_f64(),
        });
        self.send(&Frame::json(Kind::Exec, id, &request), false)?;
        let input = stdin.unwrap_or_default();
        for chunk in input.chunks(CHUNK) {
            self.send(&Frame::new(Kind::Data, id, chunk.to_vec()), false)?;
        }
        let end = json!({"size": input.len(), "sha256": hex(&Sha256::digest(input))});
        self.send(&Frame::json(Kind::End, id, &end), false)?;
        // The end frame is the commit point: from here the command may run.
        let deadline = Instant::now() + timeout + EXIT_GRACE;
        let mut output = Vec::new();
        let mut cancel_deadline = None;
        loop {
            if cancel_deadline.is_none() && self.cancelled() {
                self.send(&Frame::new(Kind::Cancel, id, Vec::new()), true)?;
                cancel_deadline = Some(Instant::now() + CANCEL_GRACE);
            }
            if cancel_deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Err(
                    self.break_with(true, "the agent did not stop the cancelled command".into())
                );
            }
            let frame = match self.wait_tick(id, deadline, true)? {
                Some(frame) => frame,
                None => continue,
            };
            match frame.kind {
                Kind::Output => output.extend_from_slice(&frame.payload),
                Kind::Exit => {
                    let value = self.value(&frame, true)?;
                    self.last_used = Instant::now();
                    return Ok(ExecOutcome {
                        rc: value.get("rc").and_then(Value::as_i64).map(|rc| rc as i32),
                        signal: value
                            .get("signal")
                            .and_then(Value::as_i64)
                            .map(|s| s as i32),
                        timed_out: value.get("timed_out").and_then(Value::as_bool) == Some(true),
                        cancelled: value.get("cancelled").and_then(Value::as_bool) == Some(true),
                        output,
                    });
                }
                Kind::Error => return Err(self.remote(&frame, true)),
                other => return Err(self.break_with(true, format!("unexpected {other:?} frame"))),
            }
        }
    }

    /// Write `source` to the guest path `path`. When `path` is a directory,
    /// the file is written into it as `name`, as `scp` does.
    pub fn write(
        &mut self,
        source: &mut dyn Read,
        path: &str,
        name: &str,
        mode: Option<u32>,
        timeout: Duration,
    ) -> Result<Transfer, SessionError> {
        let deadline = Instant::now() + timeout;
        let id = self.begin()?;
        let request = json!({"path": path, "name": name, "mode": mode});
        self.send(&Frame::json(Kind::Write, id, &request), false)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            let count = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // The agent drops the unfinished file on cancel.
                    self.send(&Frame::new(Kind::Cancel, id, Vec::new()), false)?;
                    self.drain(id, deadline)?;
                    return Err(SessionError::Local(error.to_string()));
                }
            };
            if self.cancelled() {
                self.break_with(false, "cancelled".into());
                return Err(SessionError::Cancelled { committed: false });
            }
            hasher.update(&buffer[..count]);
            size += count as u64;
            self.send(&Frame::new(Kind::Data, id, buffer[..count].to_vec()), false)?;
        }
        let sha256 = hex(&hasher.finalize());
        let end = json!({"size": size, "sha256": sha256});
        self.send(&Frame::json(Kind::End, id, &end), false)?;
        // The end frame is the commit point: from here the file may be in place.
        let frame = self.wait(id, deadline, true)?;
        match frame.kind {
            Kind::Done => {
                let value = self.value(&frame, true)?;
                let reported = (
                    value.get("size").and_then(Value::as_u64),
                    text(&value, "sha256"),
                );
                if reported != (Some(size), sha256.clone()) {
                    return Err(self.break_with(true, "the agent wrote other bytes".into()));
                }
                self.last_used = Instant::now();
                Ok(Transfer { size, sha256 })
            }
            Kind::Error => Err(self.remote(&frame, true)),
            other => Err(self.break_with(true, format!("unexpected {other:?} frame"))),
        }
    }

    /// Read the guest file `path` into `sink`.
    pub fn read(
        &mut self,
        path: &str,
        sink: &mut dyn Write,
        timeout: Duration,
    ) -> Result<Transfer, SessionError> {
        let deadline = Instant::now() + timeout;
        let id = self.begin()?;
        self.send(&Frame::json(Kind::Read, id, &json!({"path": path})), false)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut local_error = None;
        loop {
            let frame = self.wait(id, deadline, true)?;
            match frame.kind {
                Kind::Data => {
                    if local_error.is_none() {
                        if let Err(error) = sink.write_all(&frame.payload) {
                            local_error = Some(error.to_string());
                        }
                    }
                    hasher.update(&frame.payload);
                    size += frame.payload.len() as u64;
                }
                Kind::End => {
                    let value = self.value(&frame, true)?;
                    let sha256 = hex(&hasher.finalize());
                    let reported = (
                        value.get("size").and_then(Value::as_u64),
                        text(&value, "sha256"),
                    );
                    if reported != (Some(size), sha256.clone()) {
                        return Err(self.break_with(
                            true,
                            "the bytes read do not match their end frame".into(),
                        ));
                    }
                    self.last_used = Instant::now();
                    return match local_error {
                        Some(error) => Err(SessionError::Local(error)),
                        None => Ok(Transfer { size, sha256 }),
                    };
                }
                Kind::Error => return Err(self.remote(&frame, true)),
                other => return Err(self.break_with(true, format!("unexpected {other:?} frame"))),
            }
        }
    }

    /// Write the host file `local` to the guest path `remote`, as
    /// `scp local user@ip:remote` does for a file.
    pub fn write_from_path(
        &mut self,
        local: &Path,
        remote: &str,
        timeout: Duration,
    ) -> Result<Transfer, SessionError> {
        let local_error =
            |error: io::Error| SessionError::Local(format!("{}: {error}", local.display()));
        let metadata = std::fs::metadata(local).map_err(local_error)?;
        if metadata.is_dir() {
            return Err(SessionError::Local(format!(
                "{} is a directory",
                local.display()
            )));
        }
        let mut file = File::open(local).map_err(local_error)?;
        let name = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mode = metadata.permissions().mode() & 0o777;
        self.write(&mut file, remote, &name, Some(mode), timeout)
    }

    /// Read the guest file `remote` to the host path `local`, as
    /// `scp user@ip:remote local` does for a file. The file appears at its
    /// destination only when every byte arrived and matched; a failed read
    /// leaves nothing there.
    pub fn read_to_path(
        &mut self,
        remote: &str,
        local: &Path,
        timeout: Duration,
    ) -> Result<Transfer, SessionError> {
        let destination = if local.is_dir() {
            let name = Path::new(remote)
                .file_name()
                .ok_or_else(|| SessionError::Local(format!("{remote} has no file name")))?;
            local.join(name)
        } else {
            local.to_path_buf()
        };
        let temporary = temporary_sibling(&destination)?;
        let mut file =
            File::create(&temporary).map_err(|error| SessionError::Local(error.to_string()))?;
        let result = self.read(remote, &mut file, timeout).and_then(|transfer| {
            file.sync_all()
                .and_then(|()| std::fs::rename(&temporary, &destination))
                .map_err(|error| SessionError::Local(error.to_string()))?;
            Ok(transfer)
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    /// End the session: the agent sees its input close and stops everything
    /// it started.
    pub fn close(self) {}

    fn begin(&mut self) -> Result<u64, SessionError> {
        // Frames left from an earlier request are stale; an end means the
        // agent is gone, which is known before anything is sent.
        loop {
            match self.frames.try_recv() {
                Ok(Incoming::Frame(_)) => {}
                Ok(Incoming::End(reason)) => {
                    let reason = self.silent().unwrap_or(reason);
                    self.break_with(false, reason);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.break_with(false, "the session reader stopped".into());
                    break;
                }
            }
        }
        if self.cancelled() {
            self.break_with(false, "cancelled".into());
            return Err(SessionError::Cancelled { committed: false });
        }
        if let Some(reason) = self.silent() {
            self.break_with(false, reason);
        }
        if let Some(reason) = &self.broken {
            return Err(SessionError::Lost {
                committed: false,
                reason: format!("session already lost: {reason}"),
            });
        }
        let id = self.next;
        self.next += 1;
        Ok(id)
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    fn send(&mut self, frame: &Frame, committed: bool) -> Result<(), SessionError> {
        write_frame(&mut self.writer, frame)
            .map_err(|error| self.break_with(committed, format!("sending failed: {error}")))
    }

    /// The next frame of request `id`. A set cancellation flag closes the
    /// session; exec handles cancellation itself, through `wait_tick`.
    fn wait(&mut self, id: u64, deadline: Instant, committed: bool) -> Result<Frame, SessionError> {
        loop {
            if self.cancelled() {
                self.break_with(committed, "cancelled".into());
                return Err(SessionError::Cancelled { committed });
            }
            if let Some(frame) = self.wait_tick(id, deadline, committed)? {
                return Ok(frame);
            }
        }
    }

    /// The next frame of request `id`, or `None` after one tick without one.
    fn wait_tick(
        &mut self,
        id: u64,
        deadline: Instant,
        committed: bool,
    ) -> Result<Option<Frame>, SessionError> {
        if let Some(reason) = self.silent() {
            return Err(self.break_with(committed, reason));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.break_with(committed, "no answer before the deadline".into()));
        }
        match self.frames.recv_timeout(remaining.min(TICK)) {
            // Frames of an earlier, abandoned request are skipped.
            Ok(Incoming::Frame(frame)) if frame.request == id => Ok(Some(frame)),
            Ok(Incoming::Frame(_)) | Err(RecvTimeoutError::Timeout) => Ok(None),
            Ok(Incoming::End(reason)) => {
                // The silence watchdog ends the channel; say why it ended.
                let reason = self.silent().unwrap_or(reason);
                Err(self.break_with(committed, reason))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.break_with(committed, "the session reader stopped".into()))
            }
        }
    }

    /// Skip the rest of request `id` after the agent answered it.
    fn drain(&mut self, id: u64, deadline: Instant) -> Result<(), SessionError> {
        loop {
            let frame = self.wait(id, deadline, false)?;
            if matches!(
                frame.kind,
                Kind::Error | Kind::Done | Kind::End | Kind::Exit
            ) {
                return Ok(());
            }
        }
    }

    fn value(&mut self, frame: &Frame, committed: bool) -> Result<Value, SessionError> {
        frame
            .value()
            .map_err(|error| self.break_with(committed, error.to_string()))
    }

    fn remote(&mut self, frame: &Frame, committed: bool) -> SessionError {
        match self.value(frame, committed) {
            Ok(value) => {
                self.last_used = Instant::now();
                SessionError::Remote {
                    code: text(&value, "code"),
                    message: text(&value, "message"),
                }
            }
            Err(error) => error,
        }
    }

    /// Mark the session lost and close its input, so the agent stops what it
    /// started.
    fn break_with(&mut self, committed: bool, reason: String) -> SessionError {
        self.broken.get_or_insert_with(|| reason.clone());
        self.writer = Box::new(io::sink());
        SessionError::Lost { committed, reason }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Agent output that records when its last byte arrived, so a frame still
/// crossing a slow link does not count as silence.
struct Heard<R> {
    inner: R,
    heard: Arc<Mutex<Instant>>,
}

impl<R: Read> Read for Heard<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count > 0 {
            *self.heard.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        }
        Ok(count)
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn temporary_sibling(destination: &Path) -> Result<PathBuf, SessionError> {
    let name = destination.file_name().ok_or_else(|| {
        SessionError::Local(format!("{} has no file name", destination.display()))
    })?;
    let parent = destination.parent().unwrap_or(Path::new("."));
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    Ok(parent.join(format!(
        ".{}.guest-session-{}-{unique}.tmp",
        name.to_string_lossy(),
        std::process::id()
    )))
}
