//! The guest side of a guest session (`docs/arch/guest-session.md`, GS-3).
//!
//! [`serve`] reads frames from the host and answers them until the input
//! ends. Each request has one commit point, the host's end frame: a command
//! starts only after its standard input arrived whole and matched its
//! SHA-256, and a written file is renamed into place only after its bytes
//! matched. When the input ends or a frame is malformed, every running
//! command's process group is killed and every unfinished file is removed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::frame::{read_frame, write_frame, Frame, FrameError, Kind};

/// The protocol version the agent announces in its hello frame.
pub const PROTOCOL: u64 = 2;
/// How often the agent sends an alive frame.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// The largest standard input an exec may carry.
pub const MAX_STDIN: usize = 256 << 20;
/// The size of the data frames the agent sends.
pub const CHUNK: usize = 256 << 10;

/// How the agent runs commands and resolves paths.
#[derive(Clone, Debug)]
pub struct AgentOptions {
    /// The shell that runs each command line with `-c`, as `sshd` does.
    pub shell: PathBuf,
    /// The directory relative guest paths are resolved against, as `scp` does.
    pub home: PathBuf,
    /// The SHA-256 of the agent's own executable, announced in the hello frame.
    pub build: String,
    /// The interval of alive frames, announced in the hello frame; `None`
    /// sends none.
    pub heartbeat: Option<Duration>,
}

impl AgentOptions {
    /// Options from the process environment: `$SHELL` (else `/bin/sh`),
    /// `$HOME` (else `/`), the hash of the running executable, and alive
    /// frames every [`HEARTBEAT`].
    pub fn from_environment() -> AgentOptions {
        let shell = std::env::var_os("SHELL")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let build = std::env::current_exe()
            .and_then(std::fs::read)
            .map(|bytes| hex(&Sha256::digest(bytes)))
            .unwrap_or_default();
        AgentOptions {
            shell,
            home,
            build,
            heartbeat: Some(HEARTBEAT),
        }
    }
}

/// Lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The frame writer shared by the request loop and the request threads.
#[derive(Clone)]
struct Sink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Sink {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        let mut writer = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        write_frame(&mut **writer, frame)
    }

    fn error(&self, request: u64, code: &str, message: &str) {
        let _ = self.send(&Frame::json(
            Kind::Error,
            request,
            &json!({"code": code, "message": message}),
        ));
    }
}

/// A command that is running.
struct Running {
    group: i32,
    cancelled: AtomicBool,
}

/// A request waiting for its end frame.
enum Pending {
    Exec {
        command: String,
        connect_stdin: bool,
        timeout: Duration,
        input: Vec<u8>,
    },
    Write {
        temporary: PathBuf,
        destination: PathBuf,
        mode: Option<u32>,
        file: File,
        hasher: Sha256,
        size: u64,
    },
}

/// Serve one guest session until `input` ends.
///
/// Returns an error when a frame is malformed or the host can no longer be
/// written to; either way the session is over and everything it started has
/// been stopped.
pub fn serve(
    mut input: impl Read,
    output: impl Write + Send + 'static,
    options: AgentOptions,
) -> Result<(), FrameError> {
    let sink = Sink(Arc::new(Mutex::new(Box::new(output))));
    let running: Arc<Mutex<HashMap<u64, Arc<Running>>>> = Arc::default();
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let heartbeat_ms = options
        .heartbeat
        .map(|interval| interval.as_millis() as u64);
    sink.send(&Frame::json(
        Kind::Hello,
        0,
        &json!({
            "protocol": PROTOCOL,
            "build": options.build,
            "os": std::env::consts::OS,
            "heartbeat_ms": heartbeat_ms,
        }),
    ))?;
    let (stop, stopped) = mpsc::channel::<()>();
    if let Some(interval) = options.heartbeat {
        let sink = sink.clone();
        thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                if sink.send(&Frame::new(Kind::Alive, 0, Vec::new())).is_err() {
                    return;
                }
            }
        });
    }
    let result = (|| -> Result<(), FrameError> {
        while let Some(frame) = read_frame(&mut input)? {
            handle(&frame, &sink, &running, &mut pending, &options)?;
        }
        Ok(())
    })();
    drop(stop);
    for (_, request) in pending.drain() {
        if let Pending::Write { temporary, .. } = request {
            let _ = std::fs::remove_file(temporary);
        }
    }
    for process in running.lock().unwrap_or_else(|p| p.into_inner()).values() {
        kill_group(process.group);
    }
    result
}

fn handle(
    frame: &Frame,
    sink: &Sink,
    running: &Arc<Mutex<HashMap<u64, Arc<Running>>>>,
    pending: &mut HashMap<u64, Pending>,
    options: &AgentOptions,
) -> Result<(), FrameError> {
    let id = frame.request;
    let busy = |pending: &HashMap<u64, Pending>| {
        pending.contains_key(&id) || running.lock().unwrap().contains_key(&id)
    };
    match frame.kind {
        Kind::Ping => sink.send(&Frame::new(Kind::Pong, id, frame.payload.clone()))?,
        Kind::Exec | Kind::Write | Kind::Read if id == 0 || busy(pending) => {
            sink.error(id, "duplicate_request", "request ID is 0 or already in use");
        }
        Kind::Exec => {
            let value = frame.value()?;
            let command = value.get("command").and_then(Value::as_str);
            let timeout = value.get("timeout_s").and_then(Value::as_f64);
            match (command, timeout) {
                (Some(command), Some(timeout)) if timeout > 0.0 && timeout.is_finite() => {
                    pending.insert(
                        id,
                        Pending::Exec {
                            command: command.to_string(),
                            connect_stdin: value.get("stdin").and_then(Value::as_bool)
                                == Some(true),
                            timeout: Duration::from_secs_f64(timeout),
                            input: Vec::new(),
                        },
                    );
                }
                _ => sink.error(
                    id,
                    "invalid_request",
                    "exec needs command and a positive timeout_s",
                ),
            }
        }
        Kind::Write => {
            let value = frame.value()?;
            match open_write(&value, &options.home, id) {
                Ok(write) => {
                    pending.insert(id, write);
                }
                Err((code, message)) => sink.error(id, code, &message),
            }
        }
        Kind::Read => {
            let value = frame.value()?;
            match value.get("path").and_then(Value::as_str) {
                Some(path) => {
                    let path = resolve(path, &options.home);
                    let sink = sink.clone();
                    thread::spawn(move || read_file(&sink, id, &path));
                }
                None => sink.error(id, "invalid_request", "read needs path"),
            }
        }
        Kind::Data => match pending.get_mut(&id) {
            Some(Pending::Exec { input, .. }) => {
                if input.len() + frame.payload.len() > MAX_STDIN {
                    pending.remove(&id);
                    sink.error(id, "too_large", "standard input exceeds the limit");
                } else {
                    input.extend_from_slice(&frame.payload);
                }
            }
            Some(Pending::Write {
                file, hasher, size, ..
            }) => {
                if let Err(error) = file.write_all(&frame.payload) {
                    if let Some(Pending::Write { temporary, .. }) = pending.remove(&id) {
                        let _ = std::fs::remove_file(temporary);
                    }
                    sink.error(id, "write_failed", &error.to_string());
                } else {
                    hasher.update(&frame.payload);
                    *size += frame.payload.len() as u64;
                }
            }
            // A request that already failed may still have data in flight.
            None => {}
        },
        Kind::End => {
            let value = frame.value()?;
            let declared = (
                value.get("size").and_then(Value::as_u64),
                value
                    .get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            );
            match pending.remove(&id) {
                Some(Pending::Exec {
                    command,
                    connect_stdin,
                    timeout,
                    input,
                }) => {
                    let actual = (Some(input.len() as u64), Some(hex(&Sha256::digest(&input))));
                    if actual != declared {
                        sink.error(
                            id,
                            "checksum_mismatch",
                            "standard input does not match its end frame",
                        );
                    } else {
                        let stdin = connect_stdin.then_some(input);
                        start_exec(sink, running, id, options, &command, stdin, timeout);
                    }
                }
                Some(Pending::Write {
                    temporary,
                    destination,
                    mode,
                    file,
                    hasher,
                    size,
                }) => {
                    let digest = hex(&hasher.finalize());
                    let finished = (|| -> Result<(), (&str, String)> {
                        if (Some(size), Some(digest.clone())) != declared {
                            return Err((
                                "checksum_mismatch",
                                "file does not match its end frame".into(),
                            ));
                        }
                        file.sync_all()
                            .map_err(|e| ("write_failed", e.to_string()))?;
                        drop(file);
                        if let Some(mode) = mode {
                            std::fs::set_permissions(
                                &temporary,
                                std::fs::Permissions::from_mode(mode & 0o7777),
                            )
                            .map_err(|e| ("write_failed", e.to_string()))?;
                        }
                        std::fs::rename(&temporary, &destination)
                            .map_err(|e| ("write_failed", e.to_string()))
                    })();
                    match finished {
                        Ok(()) => sink.send(&Frame::json(
                            Kind::Done,
                            id,
                            &json!({"size": size, "sha256": digest}),
                        ))?,
                        Err((code, message)) => {
                            let _ = std::fs::remove_file(&temporary);
                            sink.error(id, code, &message);
                        }
                    }
                }
                None => {}
            }
        }
        Kind::Cancel => {
            if let Some(process) = running.lock().unwrap().get(&id) {
                process.cancelled.store(true, Ordering::SeqCst);
                kill_group(process.group);
            } else if let Some(request) = pending.remove(&id) {
                if let Pending::Write { temporary, .. } = request {
                    let _ = std::fs::remove_file(temporary);
                }
                sink.error(id, "cancelled", "cancelled before its end frame");
            }
        }
        Kind::Hello
        | Kind::Pong
        | Kind::Alive
        | Kind::Output
        | Kind::Exit
        | Kind::Done
        | Kind::Error => {
            sink.error(
                id,
                "unexpected_frame",
                "this frame type is sent by the agent, not to it",
            );
        }
    }
    Ok(())
}

/// Resolve a guest path the way `scp` resolves a remote path: `~` and
/// relative paths are taken from the home directory.
pub fn resolve(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        home.join(path)
    }
}

fn open_write(value: &Value, home: &Path, id: u64) -> Result<Pending, (&'static str, String)> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or(("invalid_request", "write needs path".to_string()))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mode = value
        .get("mode")
        .and_then(Value::as_u64)
        .map(|mode| mode as u32);
    let mut destination = resolve(path, home);
    // `scp` copies into an existing directory under the source's own name.
    if destination.is_dir() {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err((
                "invalid_request",
                "a directory destination needs a plain file name".into(),
            ));
        }
        destination = destination.join(name);
    }
    let parent = destination
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or((
            "not_found",
            format!("no such directory for {}", destination.display()),
        ))?;
    let file_name = destination.file_name().ok_or((
        "invalid_request",
        "destination has no file name".to_string(),
    ))?;
    let temporary = parent.join(format!(
        ".{}.guest-session-{}-{id}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let file = File::create(&temporary).map_err(|error| ("write_failed", error.to_string()))?;
    Ok(Pending::Write {
        temporary,
        destination,
        mode,
        file,
        hasher: Sha256::new(),
        size: 0,
    })
}

fn read_file(sink: &Sink, id: u64, path: &Path) {
    let mut file = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return sink.error(
                id,
                "is_directory",
                &format!("{} is a directory", path.display()),
            )
        }
        Ok(_) => match File::open(path) {
            Ok(file) => file,
            Err(error) => return sink.error(id, "read_failed", &error.to_string()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return sink.error(id, "not_found", &format!("no such file {}", path.display()))
        }
        Err(error) => return sink.error(id, "read_failed", &error.to_string()),
    };
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
                size += count as u64;
                if sink
                    .send(&Frame::new(Kind::Data, id, buffer[..count].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return sink.error(id, "read_failed", &error.to_string()),
        }
    }
    let _ = sink.send(&Frame::json(
        Kind::End,
        id,
        &json!({"size": size, "sha256": hex(&hasher.finalize())}),
    ));
}

fn start_exec(
    sink: &Sink,
    running: &Arc<Mutex<HashMap<u64, Arc<Running>>>>,
    id: u64,
    options: &AgentOptions,
    command: &str,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) {
    let (mut reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => return sink.error(id, "spawn_failed", &error.to_string()),
    };
    let mut process = Command::new(&options.shell);
    // `sshd` names the shell by its file name, which the shell then uses in
    // its own messages ("bash: line 1: ...").
    if let Some(name) = options.shell.file_name() {
        process.arg0(name);
    }
    process
        .arg("-c")
        .arg(command)
        .current_dir(&options.home)
        .process_group(0);
    process.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    match writer.try_clone() {
        Ok(copy) => process.stdout(copy).stderr(writer),
        Err(error) => return sink.error(id, "spawn_failed", &error.to_string()),
    };
    let spawned = process.spawn();
    // The command holds the parent's copies of the pipe's write end; drop them
    // so the reader sees the end of output when the command's group exits.
    drop(process);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return sink.error(id, "spawn_failed", &error.to_string()),
    };
    let process = Arc::new(Running {
        group: child.id() as i32,
        cancelled: AtomicBool::new(false),
    });
    running.lock().unwrap().insert(id, Arc::clone(&process));
    if let (Some(bytes), Some(mut input)) = (stdin, child.stdin.take()) {
        thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
    }
    let sink = sink.clone();
    let running = Arc::clone(running);
    thread::spawn(move || {
        let (finished, watch) = mpsc::channel::<()>();
        let timed_out = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let timed_out = Arc::clone(&timed_out);
            let group = process.group;
            thread::spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = watch.recv_timeout(timeout) {
                    timed_out.store(true, Ordering::SeqCst);
                    kill_group(group);
                }
            })
        };
        let mut buffer = vec![0u8; 64 << 10];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sink
                        .send(&Frame::new(Kind::Output, id, buffer[..count].to_vec()))
                        .is_err()
                    {
                        kill_group(process.group);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let status = child.wait();
        let _ = finished.send(());
        let _ = watchdog.join();
        running.lock().unwrap().remove(&id);
        let (rc, signal) = match status {
            Ok(status) => (status.code(), status.signal()),
            Err(_) => (None, None),
        };
        let _ = sink.send(&Frame::json(
            Kind::Exit,
            id,
            &json!({
                "rc": rc,
                "signal": signal,
                "timed_out": timed_out.load(Ordering::SeqCst),
                "cancelled": process.cancelled.load(Ordering::SeqCst),
            }),
        ));
    });
}

fn kill_group(group: i32) {
    if group > 0 {
        // SAFETY: `killpg` takes plain integers and has no memory effects.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}
