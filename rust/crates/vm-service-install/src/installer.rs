//! The installer's argument parsing, validation and launchd lifecycle.
//!
//! The Python source of truth is `bin/install-vm-service.sh`. The shell wrapper
//! only locates a Python interpreter and forwards `$SCRIPT_DIR` and the
//! operator's arguments to an embedded program; this module carries the whole
//! program. The deviations forced by a native service are documented in the
//! crate report: the service is launched directly instead of through an
//! interpreter, and the service-helper check validates sibling executables
//! instead of parsing Python sources.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use serde_json::{json, Map, Value};

use crate::plist;

/// The launchd label that identifies the installed agent.
pub const LABEL: &str = "com.wezzard.vm-service";

/// The login environment written into the plist unless the caller overrides it.
const DEFAULT_PATH: &str =
    "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Sibling executables the daemon needs at run time. The Python installer
/// parsed the matching Python sources; the native service ships binaries.
///
/// `guest-console-agent-linux` is required in addition to the host-native
/// `guest-console-agent`, because the agent is delivered into the guest as
/// machine code: a Linux guest cannot execute the macOS build. Without the
/// kind-qualified artifact, `--vnc` on a Linux image fails at the probe and
/// rolls the lease back. `console::guest::agent_path_for` prefers the
/// kind-qualified name and falls back to the host-native one, which still
/// serves guests whose OS matches the host.
const SERVICE_HELPERS: [&str; 5] = [
    "vm-service",
    "console-worker",
    "guest-console-agent",
    "guest-console-agent-linux",
    "vmctl",
];

/// Caller variables that select a service location and must survive into the
/// installed environment.
const CALLER_KEYS: [&str; 7] = [
    "TART",
    "TART_HOME",
    "PILOT_REPO",
    "PILOT_IMAGES_STATE_DIR",
    "VM_RELAY_STATE_DIR",
    "VM_RELAY_URL",
    "VMCTL",
];

/// A refusal that reaches the operator as `vm-service installation refused: ...`.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A validation or lifecycle failure with the exact Python message.
    #[error("{0}")]
    Refused(String),
    /// A raw filesystem or subprocess failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A selected-environment failure, reported with the Python message.
    #[error("{0}")]
    Environment(#[from] environment::EnvironmentError),
    /// A console-configuration failure, reported with the Python message.
    #[error("{0}")]
    Console(#[from] console::config::ConsoleConfigError),
}

impl InstallError {
    fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }
}

/// The install/remove action, mirroring the argparse `choices`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Action {
    /// Install or replace the LaunchAgent.
    Install,
    /// Remove the LaunchAgent.
    Remove,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Remove => "remove",
        }
    }
}

/// The parsed command line.
#[derive(Parser, Debug)]
#[command(about = "Install the stock-Tart vm-service LaunchAgent.")]
pub struct Cli {
    /// Action to perform.
    #[arg(value_enum, default_value = "install")]
    pub action: Action,
    /// Trusted console configuration to persist for the service.
    #[arg(long, value_name = "PATH")]
    pub console_config: Option<String>,
    /// Validate only; do not write or call launchctl.
    #[arg(long)]
    pub check: bool,
    /// Equivalent to `--check`.
    #[arg(long)]
    pub dry_run: bool,
}

/// Entry point used by the `vm-service-install` binary.
pub fn main_entry() -> Result<(), InstallError> {
    let cli = Cli::parse();
    let source = source_directory()?;
    run(&cli, &source, &home_directory())
}

fn source_directory() -> Result<PathBuf, InstallError> {
    let executable = std::env::current_exe()?;
    Ok(executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".")))
}

fn home_directory() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    // SAFETY: `getpwuid` returns a pointer into static storage; it is read
    // immediately and never retained.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_dir.is_null() {
            let directory = std::ffi::CStr::from_ptr((*entry).pw_dir);
            return PathBuf::from(directory.to_string_lossy().into_owned());
        }
    }
    PathBuf::from("/")
}

/// Perform one install/remove run. `source` is the directory that holds the
/// service binaries (the shell script's `SCRIPT_DIR`).
pub fn run(cli: &Cli, source: &Path, home: &Path) -> Result<(), InstallError> {
    let action = cli.action;
    let readonly = cli.check || cli.dry_run;
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));

    let old = match load_old_plist(&plist)? {
        Some(value) => value,
        None => Map::new(),
    };
    let old_has_entries = !old.is_empty();
    let previous = installed_environment(&old)?;

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PATH".to_string(), DEFAULT_PATH.to_string());
    env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    env.insert("VM_SERVICE_HOST".to_string(), "127.0.0.1".to_string());
    for (key, value) in &previous {
        env.insert(key.clone(), value.clone());
    }
    // Never copy the caller's complete environment into launchd; only the
    // documented service selectors.
    for (key, value) in std::env::vars() {
        if key.starts_with("VM_SERVICE_")
            || key.starts_with("VM_ENVIRONMENT_")
            || CALLER_KEYS.contains(&key.as_str())
        {
            env.insert(key, value);
        }
    }
    env.remove("VM_SERVICE_PYTHON");
    if let Some(config) = &cli.console_config {
        env.insert("VM_SERVICE_CONSOLE_CONFIG".to_string(), config.clone());
    }

    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    roots.insert(state_root(&env, home)?);
    let mut owners = ownership_paths(&env, home)?;
    if old_has_entries {
        let mut old_env: BTreeMap<String, String> = BTreeMap::new();
        old_env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        for (key, value) in &previous {
            old_env.insert(key.clone(), value.clone());
        }
        roots.insert(state_root(&old_env, home)?);
        owners.extend(ownership_paths(&old_env, home)?);
    }
    refuse_leases(&roots)?;

    let payload = if action == Action::Install {
        Some(validate_install(cli, &old, &previous, &env, home, source)?)
    } else {
        None
    };

    if readonly {
        println!(
            "Check passed; no files written and no launchctl calls ({}).",
            action.as_str()
        );
        return Ok(());
    }

    // Hold the service state lock through the restart so a new pending
    // acquisition cannot be persisted between the final lease check and unload.
    let mut _locks: Vec<File> = Vec::new();
    for root in &roots {
        if action == Action::Remove && !root.exists() {
            continue;
        }
        std::fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // The daemon's lock file must never be truncated while held.
            .truncate(false)
            .mode(0o600)
            .open(root.join("state.lock"))?;
        // SAFETY: `flock` operates on the owned descriptor and only sets a lock.
        let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err(InstallError::refused(format!(
                "cannot acquire the service state lock at {}",
                root.join("state.lock").display()
            )));
        }
        _locks.push(lock);
    }
    refuse_leases(&roots)?;
    stop_existing(&plist, &owners)?;

    if action == Action::Remove {
        match std::fs::remove_file(&plist) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        println!("removed {}", plist.display());
        return Ok(());
    }

    let payload = payload.expect("install payload is computed for the install action");
    let parent = plist
        .parent()
        .ok_or_else(|| InstallError::refused("invalid LaunchAgent path"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{LABEL}."))
        .suffix(".plist")
        .tempfile_in(parent)?;
    temporary.write_all(&payload)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&plist)
        .map_err(|error| InstallError::Io(error.error))?;
    // The old job and daemon were both confirmed stopped before the replace.
    launchctl(&["load"], &plist)?;

    println!("loaded {LABEL}");
    println!("logs: /tmp/{LABEL}.out / /tmp/{LABEL}.err");
    println!("state: {}", state_root(&env, home)?.display());
    Ok(())
}

fn load_old_plist(plist: &Path) -> Result<Option<Map<String, Value>>, InstallError> {
    if !plist.exists() {
        return Ok(None);
    }
    let data = std::fs::read(plist)?;
    let value = plist::parse(&data).map_err(InstallError::Refused)?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(InstallError::refused(
            "installed LaunchAgent plist is not a dictionary",
        )),
    }
}

fn installed_environment(
    old: &Map<String, Value>,
) -> Result<BTreeMap<String, String>, InstallError> {
    let mut previous = BTreeMap::new();
    match old.get("EnvironmentVariables") {
        None => {}
        Some(Value::Object(map)) => {
            for (key, value) in map {
                match value.as_str() {
                    Some(text) => {
                        previous.insert(key.clone(), text.to_string());
                    }
                    None => {
                        return Err(InstallError::refused(
                            "installed environment is not a string dictionary",
                        ))
                    }
                }
            }
        }
        Some(_) => {
            return Err(InstallError::refused(
                "installed environment is not a string dictionary",
            ))
        }
    }
    Ok(previous)
}

fn state_root(env: &BTreeMap<String, String>, home: &Path) -> Result<PathBuf, InstallError> {
    let bundle = environment::load_environment_with(None, &as_hash_map(env))?;
    if let Some(bundle) = bundle {
        let directory = bundle
            .get("profile")
            .and_then(|profile| profile.get("serviceStateDir"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                InstallError::refused("selected environment is missing serviceStateDir")
            })?;
        return Ok(PathBuf::from(directory));
    }
    Ok(env
        .get("VM_SERVICE_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state/vm-service")))
}

fn ownership_paths(
    env: &BTreeMap<String, String>,
    home: &Path,
) -> Result<BTreeSet<PathBuf>, InstallError> {
    let bundle = environment::load_environment_with(None, &as_hash_map(env))?;
    let mut paths = BTreeSet::new();
    paths.insert(state_root(env, home)?.join("daemon.lock"));
    if let Some(bundle) = bundle {
        let tart_home = bundle
            .get("profile")
            .and_then(|profile| profile.get("tartHome"))
            .and_then(Value::as_str)
            .ok_or_else(|| InstallError::refused("selected environment is missing tartHome"))?;
        paths.insert(PathBuf::from(tart_home).join(".vm-service-daemon.lock"));
    }
    Ok(paths)
}

fn refuse_leases(roots: &BTreeSet<PathBuf>) -> Result<(), InstallError> {
    for root in roots {
        let state = root.join("state.json");
        let data = match std::fs::read(&state) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let parsed: Value = serde_json::from_slice(&data)
            .map_err(|error| InstallError::Refused(error.to_string()))?;
        if !parsed.is_object() {
            return Err(InstallError::refused(
                "cannot establish lease safety from service state",
            ));
        }
        let vms = parsed
            .get("vms")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                InstallError::refused("cannot establish lease safety from service state")
            })?;
        // Retained failed/deleting records still own resources. Never use TTL
        // or the running-state subset to infer that restarting is harmless.
        if !vms.is_empty() {
            return Err(InstallError::refused(
                "active or retained leases exist; release them before install/remove",
            ));
        }
    }
    Ok(())
}

fn validate_install(
    cli: &Cli,
    old: &Map<String, Value>,
    previous: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    _home: &Path,
    source: &Path,
) -> Result<Vec<u8>, InstallError> {
    let service = source.join("vm-service");
    if !service.is_file() || !is_executable(&service) {
        return Err(InstallError::refused(format!(
            "missing executable service: {}",
            service.display()
        )));
    }
    for name in SERVICE_HELPERS {
        let helper = source.join(name);
        if !helper.is_file() {
            return Err(InstallError::refused(format!(
                "missing service helper: {}",
                helper.display()
            )));
        }
        if !is_executable(&helper) {
            return Err(InstallError::refused(format!(
                "service helper is not executable: {}",
                helper.display()
            )));
        }
    }

    let env_map = as_hash_map(env);
    let config = console::config::load_config(None, Some(&env_map))?
        .unwrap_or_else(console::config::ConsoleConfig::disabled);
    if config.enabled {
        let previous_console = previous
            .get("VM_SERVICE_CONSOLE_CONFIG")
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        if !previous_console && cli.console_config.is_none() {
            return Err(InstallError::refused(
                "initial console enablement requires --console-config PATH",
            ));
        }
        let available = console::config::availability("linux", Some(&config))
            || console::config::availability("macos", Some(&config));
        if !available {
            return Err(InstallError::refused(
                "enabled console configuration has no usable host viewer",
            ));
        }
    }

    let bundle = environment::load_environment_with(None, &env_map)?;
    let mut effective = env.clone();
    if let Some(bundle) = &bundle {
        if let Some(environment) = bundle.get("environment").and_then(Value::as_object) {
            for (key, value) in environment {
                if let Some(text) = value.as_str() {
                    effective.insert(key.clone(), text.to_string());
                }
            }
        }
    }
    let tart = effective
        .get("TART")
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| which("tart", effective.get("PATH").map(String::as_str)));
    let tart = match tart {
        Some(path) if Path::new(&path).is_file() && is_executable(Path::new(&path)) => path,
        _ => {
            return Err(InstallError::refused(
                "stock Tart executable is missing; install an unmodified upstream release",
            ))
        }
    };

    let mut command = Command::new(&tart);
    command.arg("--version");
    command.env_clear();
    for (key, value) in &effective {
        command.env(key, value);
    }
    let output = run_command(&mut command, Duration::from_secs(10))?;
    if !output.status.success() {
        return Err(InstallError::refused(format!(
            "Tart version check failed with {}",
            describe_status(&output.status)
        )));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // A version command is an observation, not proof of vendor provenance,
    // host boot compatibility, image readiness, or successful VNC viewing.
    if !matches_tart_version(&version) {
        return Err(InstallError::refused(
            "Tart did not report a plain upstream release version",
        ));
    }

    let mut data = old.clone();
    data.insert("Label".to_string(), Value::String(LABEL.to_string()));
    data.insert(
        "ProgramArguments".to_string(),
        json!([service.to_string_lossy().into_owned()]),
    );
    data.insert("EnvironmentVariables".to_string(), environment_value(env));
    data.insert("RunAtLoad".to_string(), Value::Bool(true));
    data.insert("KeepAlive".to_string(), Value::Bool(true));
    data.entry("StandardOutPath".to_string())
        .or_insert_with(|| Value::String(format!("/tmp/{LABEL}.out")));
    data.entry("StandardErrorPath".to_string())
        .or_insert_with(|| Value::String(format!("/tmp/{LABEL}.err")));

    println!(
        "Tart version observed: {version}. Upstream provenance and guest compatibility \
         require acceptance checks."
    );
    if config.enabled {
        println!(
            "Console host configuration validated. Guest sharing, image compatibility, \
             and viewing remain unverified."
        );
    }
    Ok(plist::dumps(&Value::Object(data)))
}

fn stop_existing(plist: &Path, owners: &BTreeSet<PathBuf>) -> Result<(), InstallError> {
    let pid = launch_job(plist)?;
    let Some(pid) = pid else {
        if daemon_owned(owners)? {
            return Err(InstallError::refused(
                "daemon ownership remains without a launchctl job; refusing replacement/removal",
            ));
        }
        return Ok(());
    };
    if !plist.is_file() {
        return Err(InstallError::refused(
            "loaded job has no installed plist; reconcile it before installation",
        ));
    }
    let mut unload = Command::new("launchctl");
    unload.arg("unload").arg(plist);
    let result = run_command(&mut unload, Duration::from_secs(10))?;
    if !result.status.success() {
        return Err(InstallError::refused(
            "launchctl unload failed; installed plist preserved and no replacement started",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Job deregistration alone is not proof that its daemon exited.
        if launch_job(plist)?.is_none() && !daemon_owned(owners)? && !pid_alive(pid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(InstallError::refused(
                "job or daemon remains after unload; installed plist preserved",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn launch_job(plist: &Path) -> Result<Option<i32>, InstallError> {
    // SAFETY: `getuid` takes no arguments and only reads process state.
    let uid = unsafe { libc::getuid() };
    let mut jobs: Vec<i32> = Vec::new();
    for domain in ["gui", "user"] {
        let target = format!("{domain}/{uid}/{LABEL}");
        let mut command = Command::new("launchctl");
        command.arg("print").arg(&target);
        let output = run_command(&mut command, Duration::from_secs(10))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(113) && stderr.contains("Could not find service") {
            continue;
        }
        if !output.status.success() {
            return Err(InstallError::refused(
                "cannot establish launchctl job state; refusing to change installation",
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        match find_line_value(&stdout, "path") {
            Some(path) if Path::new(&path) == plist => {}
            _ => {
                return Err(InstallError::refused(
                    "launchctl job does not identify the installed plist; refusing to unload",
                ))
            }
        }
        let pid = find_line_value(&stdout, "pid").and_then(|value| value.parse::<i32>().ok());
        jobs.push(pid.unwrap_or(0));
    }
    if jobs.len() > 1 {
        return Err(InstallError::refused(
            "multiple launchctl jobs exist; reconcile ownership before installation",
        ));
    }
    Ok(jobs.first().copied())
}

fn find_line_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = ");
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let rest = rest.trim_end_matches('\r');
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn daemon_owned(paths: &BTreeSet<PathBuf>) -> Result<bool, InstallError> {
    // Inspect the same singleton locks as the daemon, including the selected
    // Tart store. Do not create, truncate, replace, or unlink these lock files.
    for path in paths {
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| InstallError::refused("invalid lock path"))?;
        // SAFETY: `open` reads the NUL-terminated path and returns a descriptor
        // that is either used below or reported.
        let descriptor = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            return Err(error.into());
        }
        // SAFETY: `descriptor` was just returned by `open` and is not owned elsewhere.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(InstallError::refused(format!(
                "nonregular daemon ownership lock: {}",
                path.display()
            )));
        }
        // SAFETY: `flock` operates on the owned descriptor and only sets a lock.
        let result = unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                return Ok(true);
            }
            return Err(error.into());
        }
        // Dropping `file` releases the probe lock.
    }
    Ok(false)
}

fn pid_alive(pid: i32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs the existence check only; the installer never
    // terminates a process.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn launchctl(operands: &[&str], plist: &Path) -> Result<(), InstallError> {
    let mut command = Command::new("launchctl");
    for operand in operands {
        command.arg(operand);
    }
    command.arg(plist);
    let output = run_command(&mut command, Duration::from_secs(10))?;
    if !output.status.success() {
        return Err(InstallError::refused(format!(
            "launchctl {} failed; the installed plist was preserved",
            operands.join(" ")
        )));
    }
    Ok(())
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run a command with the Python `subprocess.run(..., timeout=...)` semantics:
/// capture both streams, and kill the child when the deadline expires.
fn run_command(command: &mut Command, timeout: Duration) -> Result<CommandOutput, InstallError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| InstallError::refused("cannot capture subprocess stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| InstallError::refused("cannot capture subprocess stderr"))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InstallError::refused(format!(
                "subprocess timed out after {} seconds",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(CommandOutput {
        status,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

fn describe_status(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "a signal".to_string(),
    }
}

/// `shutil.which` for a single name over an explicit `PATH`.
fn which(name: &str, path: Option<&str>) -> Option<String> {
    let path = path?;
    for directory in path.split(':') {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    match CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: `access` reads the NUL-terminated path and only tests it.
        Ok(cpath) => unsafe { libc::access(cpath.as_ptr(), libc::X_OK) == 0 },
        Err(_) => false,
    }
}

fn as_hash_map(env: &BTreeMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn environment_value(env: &BTreeMap<String, String>) -> Value {
    let mut map = Map::new();
    for (key, value) in env {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

/// Full-match the Python `(?:tart\s+)?\d+\.\d+\.\d+(?:\s+\([A-Za-z0-9 ._-]+\))?`
/// pattern, case-insensitively.
fn matches_tart_version(input: &str) -> bool {
    let mut rest = input;
    if rest.len() >= 4 && rest.as_bytes()[..4].eq_ignore_ascii_case(b"tart") {
        let after = &rest[4..];
        let whitespace = after.len() - after.trim_start_matches(char::is_whitespace).len();
        if whitespace > 0 {
            rest = &after[whitespace..];
        }
    }
    for component in 0..3 {
        let (digits, remainder) = take_digits(rest);
        if digits.is_empty() {
            return false;
        }
        rest = remainder;
        if component < 2 {
            if !rest.starts_with('.') {
                return false;
            }
            rest = &rest[1..];
        }
    }
    // After the three dotted numbers, only the optional parenthesized qualifier
    // may follow.
    if rest.is_empty() {
        return true;
    }
    let whitespace = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
    if whitespace == 0 {
        return false;
    }
    rest = &rest[whitespace..];
    if !rest.starts_with('(') || !rest.ends_with(')') {
        return false;
    }
    let inner = &rest[1..rest.len() - 1];
    if inner.is_empty() {
        return false;
    }
    inner.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || character == ' '
            || character == '.'
            || character == '_'
            || character == '-'
    })
}

fn take_digits(input: &str) -> (&str, &str) {
    let mut end = 0;
    for (index, character) in input.char_indices() {
        if character.is_ascii_digit() {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }
    input.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_upstream_versions() {
        assert!(matches_tart_version("2.32.1"));
        assert!(matches_tart_version("tart 2.32.1"));
        assert!(matches_tart_version("TART 2.32.1 (Engine)"));
        assert!(matches_tart_version("2.32.1 (a-b_c.d)"));
        assert!(matches_tart_version("2.32.1 (2.32.1)"));
    }

    #[test]
    fn rejects_patched_or_partial_versions() {
        assert!(!matches_tart_version("patched-private-build"));
        assert!(!matches_tart_version("2.32"));
        assert!(!matches_tart_version("v2.32.1"));
        assert!(!matches_tart_version("2.32.1  (bad!)"));
        assert!(!matches_tart_version(""));
    }

    #[test]
    fn finds_indented_launchctl_fields() {
        let text = "com.wezzard.vm-service = {\n\tpath = /tmp/a.plist\n\tpid = 1234\n}\n";
        assert_eq!(
            find_line_value(text, "path").as_deref(),
            Some("/tmp/a.plist")
        );
        assert_eq!(find_line_value(text, "pid").as_deref(), Some("1234"));
        assert_eq!(find_line_value(text, "missing"), None);
    }
}
