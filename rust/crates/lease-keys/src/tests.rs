//! Port of the pure `bin/lease_keys.py` unit tests.
//!
//! `tests/unit/test_key_lifecycle.py` and `tests/unit/test_key_commands.py`
//! drive the `vm-service` daemon (`_ssh`/`_scp`/`verify_transfer`, lease
//! lifecycle) and belong to the `vm-service-core` crate, not to `lease-keys`.
//! The `e2e/test_lease_keys_live.py` runner is an opt-in live harness; it has no
//! pure lease-keys assertions, so it is represented by an `#[ignore]` marker.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::unix::fs::symlink;

// ---------------------------------------------------------------------------
// Fixtures and fakes
// ---------------------------------------------------------------------------

fn temp_state() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let state = fs::canonicalize(dir.path()).unwrap();
    (dir, state)
}

fn file_mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn glob_askpass(path: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .file_name()
                .and_then(OsStr::to_str)
                .map(|name| name.starts_with(".askpass-"))
                .unwrap_or(false)
        })
        .collect();
    found.sort();
    found
}

/// Keygen seam that fails loudly if a test forgets that generation must be
/// skipped (for example when the directory already exists).
struct PanicKeygen;

impl Keygen for PanicKeygen {
    fn run(&self, _argv: &[String], _timeout: Duration) -> io::Result<RunResult> {
        panic!("ssh-keygen must not be invoked");
    }
}

/// Keygen seam that writes partial artifacts before failing, mirroring the
/// Python `test_keygen_partial_failure_and_timeout_remove_generated_files`.
struct PartialKeygen {
    timeout: bool,
}

impl Keygen for PartialKeygen {
    fn run(&self, argv: &[String], _timeout: Duration) -> io::Result<RunResult> {
        let index = argv.iter().position(|arg| arg == "-f").unwrap();
        let identity = PathBuf::from(&argv[index + 1]);
        fs::write(&identity, "partial private key").unwrap();
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
        let public = identity.with_extension("pub");
        fs::write(&public, "partial public key").unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o644)).unwrap();
        if self.timeout {
            Ok(RunResult::TimedOut)
        } else {
            Ok(RunResult::Exited {
                code: 1,
                stderr: Vec::new(),
            })
        }
    }
}

struct FakeClock {
    now: RefCell<f64>,
    sleeps: Cell<usize>,
}

impl FakeClock {
    fn new(now: f64) -> Self {
        FakeClock {
            now: RefCell::new(now),
            sleeps: Cell::new(0),
        }
    }

    fn advance(&self, secs: f64) {
        *self.now.borrow_mut() += secs;
    }

    fn value(&self) -> f64 {
        *self.now.borrow()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> f64 {
        *self.now.borrow()
    }

    fn sleep(&self, secs: f64) {
        self.sleeps.set(self.sleeps.get() + 1);
        *self.now.borrow_mut() += secs;
    }
}

#[derive(Clone)]
struct RecordedCall {
    argv: Vec<String>,
    stdin: Vec<u8>,
    env: EnvMap,
    timeout: Duration,
}

enum FakeResponse {
    Exit(i32, &'static str),
    Timeout,
    Io,
}

struct FakeSsh<'a> {
    responses: RefCell<VecDeque<FakeResponse>>,
    calls: RefCell<Vec<RecordedCall>>,
    clock: Option<&'a FakeClock>,
}

impl<'a> FakeSsh<'a> {
    fn new(responses: Vec<FakeResponse>, clock: Option<&'a FakeClock>) -> Self {
        FakeSsh {
            responses: RefCell::new(responses.into()),
            calls: RefCell::new(Vec::new()),
            clock,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.borrow().clone()
    }
}

impl SshTransport for FakeSsh<'_> {
    fn run(
        &self,
        argv: &[String],
        stdin: &[u8],
        env: &EnvMap,
        timeout: Duration,
    ) -> io::Result<RunResult> {
        self.calls.borrow_mut().push(RecordedCall {
            argv: argv.to_vec(),
            stdin: stdin.to_vec(),
            env: env.clone(),
            timeout,
        });
        let response = self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("unexpected ssh invocation");
        match response {
            FakeResponse::Exit(code, error) => Ok(RunResult::Exited {
                code,
                stderr: error.as_bytes().to_vec(),
            }),
            FakeResponse::Timeout => {
                if let Some(clock) = self.clock {
                    clock.advance(timeout.as_secs_f64());
                }
                Ok(RunResult::TimedOut)
            }
            FakeResponse::Io => Err(io::Error::other("unique-secret-42")),
        }
    }
}

fn base_env() -> EnvMap {
    let mut env = EnvMap::new();
    env.insert(
        OsString::from("SSH_AUTH_SOCK"),
        OsString::from("/fake/agent"),
    );
    env
}

// ---------------------------------------------------------------------------
// create / directory / key_args
// ---------------------------------------------------------------------------

#[test]
fn real_keygen_permissions_unique_and_no_rekey() {
    let (_tmp, state) = temp_state();
    let first = create(&state, "vm-one").unwrap();
    let second = create(&state, "vm-two").unwrap();
    for path in [&first, &second] {
        assert_eq!(path.parent().unwrap(), state.join("ssh"));
        assert_eq!(file_mode(path), 0o700);
        assert_eq!(file_mode(path.parent().unwrap()), 0o700);
        for filename in ["identity", "identity.pub", "known_hosts"] {
            let meta = fs::symlink_metadata(path.join(filename)).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o600, "{filename}");
            assert_eq!(meta.uid(), current_uid(), "{filename}");
        }
        assert_eq!(fs::read(path.join("known_hosts")).unwrap(), b"");
        let public = fs::read_to_string(path.join("identity.pub")).unwrap();
        assert!(public.starts_with("ssh-ed25519 "));
        let derived = Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(path.join("identity"))
            .output()
            .unwrap();
        let derived = String::from_utf8(derived.stdout).unwrap();
        let derived: Vec<&str> = derived.split_whitespace().take(2).collect();
        let expected: Vec<&str> = public.split_whitespace().take(2).collect();
        assert_eq!(derived, expected);
        let vm = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(directory(&state, vm).unwrap(), *path);
    }
    assert_ne!(
        fs::read(first.join("identity")).unwrap(),
        fs::read(second.join("identity")).unwrap()
    );
    let before = fs::read(first.join("identity")).unwrap();
    // The directory already exists: creation fails closed before generating.
    assert!(create_with(&state, "vm-one", &PanicKeygen).is_err());
    assert_eq!(fs::read(first.join("identity")).unwrap(), before);
}

#[test]
fn key_args_are_strict_standard_openssh() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let args = key_args(&path).unwrap();
    assert_eq!(&args[..2], ["-F", "/dev/null"]);
    let identity = args.iter().position(|arg| arg == "-i").unwrap();
    assert_eq!(args[identity + 1], path.join("identity").to_string_lossy());
    for option in [
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "BatchMode=yes",
        "PreferredAuthentications=publickey",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "StrictHostKeyChecking=yes",
        "GlobalKnownHostsFile=/dev/null",
        "ConnectTimeout=8",
        "LogLevel=ERROR",
        "ForwardAgent=no",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
    ] {
        assert!(args.iter().any(|arg| arg == option), "missing {option}");
    }
    let expected = format!(
        "UserKnownHostsFile=\"{}\"",
        path.join("known_hosts").display()
    );
    assert!(
        args.iter().any(|arg| arg == &expected),
        "missing {expected}"
    );
    assert!(!args.join(" ").contains("accept-new"));
}

#[test]
fn malicious_vm_names_and_state_paths() {
    let (_tmp, state) = temp_state();
    for name in [
        "",
        ".",
        "..",
        "../outside",
        "/absolute",
        "a/b",
        "-flag",
        "a\n",
        "a\x00",
    ] {
        assert!(create(&state, name).is_err(), "accepted vm name {name:?}");
    }
    let dotdot = state.join("..").join(state.file_name().unwrap());
    assert!(create(&dotdot, "vm").is_err());
    assert!(create(&state.join("bad\npath"), "vm").is_err());
    let alias = state.join("alias");
    symlink(&state, &alias).unwrap();
    assert!(create(&alias, "vm").is_err());
    fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(create(&state, "vm").is_err());
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn symlink_ssh_parent_and_existing_lease_fail_closed() {
    let (_tmp, state) = temp_state();
    let target = state.join("target");
    fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
    let root = state.join("ssh");
    symlink(&target, &root).unwrap();
    assert!(create(&state, "vm-one").is_err());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    fs::remove_file(&root).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    symlink(&target, root.join("vm-one")).unwrap();
    assert!(create(&state, "vm-one").is_err());
    assert!(directory(&state, "vm-one").is_err());
    assert!(cleanup(&state, "vm-one").is_err());
    assert!(fs::symlink_metadata(root.join("vm-one"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn directory_rejects_unsafe_modes_symlinks_hardlinks_and_missing_keys() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    for filename in ["identity", "identity.pub", "known_hosts"] {
        let file = path.join(filename);
        let original = fs::read(&file).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(directory(&state, "vm-one").is_err(), "{filename} mode");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        let target = state.join("outside");
        fs::write(&target, &original).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&target, &file).unwrap();
        assert!(key_args(&path).is_err(), "{filename} symlink");
        assert!(
            cleanup(&state, "vm-one").is_err(),
            "{filename} symlink cleanup"
        );
        fs::remove_file(&file).unwrap();

        fs::hard_link(&target, &file).unwrap();
        assert!(directory(&state, "vm-one").is_err(), "{filename} hardlink");
        fs::remove_file(&file).unwrap();
        assert!(directory(&state, "vm-one").is_err(), "{filename} missing");
        assert!(create_with(&state, "vm-one", &PanicKeygen).is_err());

        fs::write(&file, &original).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&target).unwrap();
    }
    let ssh_parent = path.parent().unwrap().to_path_buf();
    for folder in [&path, &ssh_parent] {
        fs::set_permissions(folder, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(directory(&state, "vm-one").is_err());
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(check_meta(&meta, &[0o700], true, current_uid().wrapping_add(1)).is_err());
    fs::write(path.join("identity"), b"").unwrap();
    assert!(directory(&state, "vm-one").is_err());
}

#[test]
fn create_failure_removes_only_its_reservation_and_cleanup_accepts_partial() {
    let (_tmp, state) = temp_state();
    let other = create(&state, "vm-other").unwrap();
    assert!(create_with(&state, "vm-one", &PartialKeygen { timeout: false }).is_err());
    let path = state.join("ssh").join("vm-one");
    assert!(!path.exists());
    assert!(other.join("identity").exists());
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    assert!(create(&state, "vm-one").is_err());
    assert!(path.exists());
    cleanup(&state, "vm-one").unwrap();
    assert!(!path.exists());
    cleanup(&state, "vm-one").unwrap();
}

#[test]
fn keygen_partial_failure_and_timeout_remove_generated_files() {
    for timeout in [false, true] {
        let (_tmp, state) = temp_state();
        assert!(create_with(&state, "vm-one", &PartialKeygen { timeout }).is_err());
        assert!(!state.join("ssh").join("vm-one").exists());
    }
}

#[test]
fn cleanup_is_bounded_and_validates_before_deletion() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let other = create(&state, "vm-two").unwrap();
    fs::write(path.join("unexpected"), "keep").unwrap();
    assert!(cleanup(&state, "vm-one").is_err());
    assert!(path.join("identity").exists());
    fs::remove_file(path.join("unexpected")).unwrap();
    cleanup(&state, "vm-one").unwrap();
    assert!(!path.exists());
    assert!(other.join("identity").exists());
    cleanup(&state, "vm-one").unwrap();
}

// ---------------------------------------------------------------------------
// provision script
// ---------------------------------------------------------------------------

#[test]
fn provision_script_is_idempotent_quotes_public_key_and_rejects_symlinks() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let public_file = path.join("identity.pub");
    let public = format!(
        "{} '$(touch INJECTED)'",
        fs::read_to_string(&public_file).unwrap().trim()
    );
    fs::write(&public_file, format!("{public}\n")).unwrap();
    fs::set_permissions(&public_file, fs::Permissions::from_mode(0o600)).unwrap();

    let home = state.join("guest");
    fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
    let script = provision_script(&path).unwrap();
    let run = |script: &str| -> std::process::Output {
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", &home)
            .current_dir(&home)
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    for _ in 0..2 {
        let output = run(&script);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let authorized = home.join(".ssh").join("authorized_keys");
    let content = fs::read_to_string(&authorized).unwrap();
    assert_eq!(content.lines().filter(|line| *line == public).count(), 1);
    assert!(!home.join("INJECTED").exists());
    assert_eq!(file_mode(&authorized), 0o600);
    assert_eq!(file_mode(&home.join(".ssh")), 0o700);

    fs::remove_file(&authorized).unwrap();
    let target = home.join("outside");
    fs::write(&target, "unchanged").unwrap();
    symlink(&target, &authorized).unwrap();
    let output = run(&script);
    assert_eq!(output.status.code(), Some(73));
    assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");

    fs::remove_file(&authorized).unwrap();
    fs::remove_dir(home.join(".ssh")).unwrap();
    symlink(&path, home.join(".ssh")).unwrap();
    let output = run(&script);
    assert_eq!(output.status.code(), Some(73));
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AskpassSsh {
    helpers: RefCell<Vec<PathBuf>>,
}

impl SshTransport for AskpassSsh {
    fn run(
        &self,
        argv: &[String],
        stdin: &[u8],
        env: &EnvMap,
        timeout: Duration,
    ) -> io::Result<RunResult> {
        assert_eq!(argv[0], "ssh");
        assert_eq!(&argv[1..3], ["-F", "/dev/null"]);
        assert_eq!(
            &argv[argv.len() - 6..],
            ["-l", "admin", "--", "192.0.2.10", "/bin/sh", "-s"]
        );
        for option in [
            "StrictHostKeyChecking=accept-new",
            "BatchMode=no",
            "PreferredAuthentications=password",
            "PubkeyAuthentication=no",
            "PasswordAuthentication=yes",
            "KbdInteractiveAuthentication=no",
            "IdentityAgent=none",
            "ForwardAgent=no",
            "NumberOfPasswordPrompts=1",
        ] {
            assert!(argv.iter().any(|arg| arg == option), "missing {option}");
        }
        assert!(!argv.iter().any(|arg| arg == "-i"));
        assert!(timeout <= Duration::from_secs(30));
        assert_eq!(
            env.get(OsStr::new("SSH_ASKPASS_REQUIRE")).unwrap(),
            OsStr::new("force")
        );
        assert!(!env.get(OsStr::new("DISPLAY")).unwrap().is_empty());
        assert!(!env.contains_key(OsStr::new("SSH_AUTH_SOCK")));

        let helper = PathBuf::from(env.get(OsStr::new("SSH_ASKPASS")).unwrap());
        self.helpers.borrow_mut().push(helper.clone());
        assert_eq!(file_mode(&helper), 0o700);
        assert!(!argv.join(" ").contains("unique-secret-42"));
        assert!(!String::from_utf8_lossy(stdin).contains("unique-secret-42"));
        for entry in fs::read_dir(helper.parent().unwrap()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                let data = fs::read(entry.path()).unwrap();
                let secret = b"unique-secret-42";
                assert!(!data.windows(secret.len()).any(|window| window == secret));
            }
        }
        let output = Command::new(&helper)
            .env_clear()
            .envs(env)
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"unique-secret-42\n");
        Ok(RunResult::Exited {
            code: 0,
            stderr: Vec::new(),
        })
    }
}

#[test]
fn bootstrap_askpass_password_only_no_secret_in_argv_or_files() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let private = fs::read(path.join("identity")).unwrap();
    let clock = FakeClock::new(0.0);
    let ssh = AskpassSsh::default();
    let base = base_env();
    let base_before = base.clone();
    bootstrap_with(
        "192.0.2.10",
        "admin",
        "unique-secret-42",
        &path,
        420.0,
        &clock,
        &ssh,
        &base,
    )
    .unwrap();
    let helpers = ssh.helpers.borrow().clone();
    assert!(!helpers.is_empty());
    for helper in helpers {
        assert!(!helper.exists(), "askpass helper leaked");
    }
    assert_eq!(fs::read(path.join("identity")).unwrap(), private);
    assert_eq!(base.len(), base_before.len());
    assert!(!base.contains_key(OsStr::new(PASSWORD_ENV)));
}

#[test]
fn bootstrap_retries_connectivity_and_auth_using_same_key() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let clock = FakeClock::new(0.0);
    let ssh = FakeSsh::new(
        vec![
            FakeResponse::Exit(255, "Connection refused"),
            FakeResponse::Exit(255, "Permission denied (password)."),
            FakeResponse::Exit(0, ""),
        ],
        None,
    );
    let base = EnvMap::new();
    bootstrap_with(
        "192.0.2.10",
        "admin",
        "unique-secret-42",
        &path,
        420.0,
        &clock,
        &ssh,
        &base,
    )
    .unwrap();
    assert_eq!(ssh.call_count(), 3);
    assert_eq!(clock.sleeps.get(), 2);
    let calls = ssh.calls();
    assert_eq!(calls[0].stdin, calls[2].stdin);
    assert_eq!(calls[0].argv, calls[2].argv);
    assert!(calls[0].env.contains_key(OsStr::new("SSH_ASKPASS")));
    assert!(glob_askpass(&path).is_empty());
}

#[test]
fn bootstrap_failures_sanitized_not_retried_and_keys_retained() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let private = fs::read(path.join("identity")).unwrap();
    let known_hosts = path.join("known_hosts");
    fs::write(&known_hosts, "192.0.2.10 ssh-ed25519 pinned-host-key\n").unwrap();
    for response in [
        FakeResponse::Exit(73, "unique-secret-42"),
        FakeResponse::Exit(255, "HOST IDENTIFICATION HAS CHANGED unique-secret-42"),
        FakeResponse::Exit(
            255,
            "Host key verification failed. Permission denied unique-secret-42",
        ),
        FakeResponse::Io,
    ] {
        let clock = FakeClock::new(0.0);
        let ssh = FakeSsh::new(vec![response], None);
        let base = EnvMap::new();
        let error = bootstrap_with(
            "192.0.2.10",
            "admin",
            "unique-secret-42",
            &path,
            420.0,
            &clock,
            &ssh,
            &base,
        )
        .unwrap_err();
        assert_eq!(ssh.call_count(), 1);
        assert!(!error.to_string().contains("unique-secret-42"));
        assert!(glob_askpass(&path).is_empty());
        assert_eq!(fs::read(path.join("identity")).unwrap(), private);
        assert_eq!(
            fs::read_to_string(&known_hosts).unwrap(),
            "192.0.2.10 ssh-ed25519 pinned-host-key\n"
        );
    }
}

#[test]
fn bootstrap_total_deadline_and_subprocess_timeouts() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let clock = FakeClock::new(100.0);
    let ssh = FakeSsh::new(
        vec![
            FakeResponse::Timeout,
            FakeResponse::Timeout,
            FakeResponse::Timeout,
        ],
        Some(&clock),
    );
    let base = EnvMap::new();
    let error = bootstrap_with(
        "192.0.2.10",
        "admin",
        "unique-secret-42",
        &path,
        35.0,
        &clock,
        &ssh,
        &base,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH bootstrap deadline exceeded");
    let timeouts: Vec<Duration> = ssh.calls().iter().map(|call| call.timeout).collect();
    assert_eq!(
        timeouts,
        vec![Duration::from_secs(30), Duration::from_secs(3)]
    );
    assert_eq!(clock.value(), 135.0);
    assert!(glob_askpass(&path).is_empty());
    assert!(path.join("identity").exists());
}

#[test]
fn refused_auth_cannot_retry_beyond_deadline() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let clock = FakeClock::new(0.0);
    let ssh = FakeSsh::new(
        vec![
            FakeResponse::Exit(255, "Permission denied (password)"),
            FakeResponse::Exit(255, "Permission denied (password)"),
            FakeResponse::Exit(255, "Permission denied (password)"),
        ],
        None,
    );
    let base = EnvMap::new();
    let error = bootstrap_with(
        "192.0.2.10",
        "admin",
        "unique-secret-42",
        &path,
        3.0,
        &clock,
        &ssh,
        &base,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH bootstrap deadline exceeded");
    assert_eq!(ssh.call_count(), 2);
    assert_eq!(clock.value(), 3.0);
}

#[test]
fn invalid_bootstrap_inputs_never_launch_ssh() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let clock = FakeClock::new(0.0);
    let ssh = FakeSsh::new(Vec::new(), None);
    let base = EnvMap::new();
    for timeout in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        assert!(
            bootstrap_with(
                "192.0.2.10",
                "admin",
                "secret",
                &path,
                timeout,
                &clock,
                &ssh,
                &base
            )
            .is_err(),
            "accepted timeout {timeout}"
        );
    }
    for (ip, user) in [
        ("-oProxyCommand=evil", "admin"),
        ("192.0.2.10", "-admin"),
        ("192.0.2.10", "admin\nanything"),
        // CPython `ipaddress.ip_address` rejects a zone on IPv4, an empty zone,
        // a zone containing `%` or `/`, and a zone on a non-literal
        // (`bin/lease_keys.py:183`).
        ("1.2.3.4%eth0", "admin"),
        ("fe80::1%", "admin"),
        ("fe80::1%eth0%x", "admin"),
        ("fe80::1%/64", "admin"),
        ("fe80::1%a/b", "admin"),
        ("www.example.com%eth0", "admin"),
        ("%eth0", "admin"),
    ] {
        assert!(
            bootstrap_with(ip, user, "secret", &path, 10.0, &clock, &ssh, &base).is_err(),
            "accepted {ip} {user:?}"
        );
    }
    for password in ["", "first\nsecond", "first\rsecond", "null\x00byte"] {
        assert!(
            bootstrap_with(
                "192.0.2.10",
                "admin",
                password,
                &path,
                10.0,
                &clock,
                &ssh,
                &base
            )
            .is_err(),
            "accepted password {password:?}"
        );
    }
    assert_eq!(ssh.call_count(), 0);
    assert!(glob_askpass(&path).is_empty());
}

/// CPython `ipaddress.ip_address` accepts a single non-empty `%zone` suffix on
/// an IPv6 literal (`bin/lease_keys.py:183`), and Python `bootstrap` then
/// forwards the original string (with its zone) to `ssh`. There is no Python
/// unit test for this input, so this test has no Python counterpart; the
/// expectation was measured against CPython 3.14.7
/// (`ipaddress.ip_address('fe80::1%eth0')` succeeds).
#[test]
fn bootstrap_accepts_zone_qualified_ipv6_literal() {
    let (_tmp, state) = temp_state();
    let path = create(&state, "vm-one").unwrap();
    let clock = FakeClock::new(0.0);
    let ssh = FakeSsh::new(vec![FakeResponse::Exit(0, "")], None);
    let base = EnvMap::new();
    bootstrap_with(
        "fe80::1%eth0",
        "admin",
        "unique-secret-42",
        &path,
        420.0,
        &clock,
        &ssh,
        &base,
    )
    .unwrap();
    assert_eq!(ssh.call_count(), 1);
    let call = &ssh.calls()[0];
    assert!(
        call.argv.iter().any(|arg| arg.as_str() == "fe80::1%eth0"),
        "zone-qualified literal must reach ssh unchanged: {:?}",
        call.argv
    );
}

// ---------------------------------------------------------------------------
// Live acceptance marker
// ---------------------------------------------------------------------------

/// The live runner in `tests/e2e/test_lease_keys_live.py` is an opt-in harness
/// bound to parent authorization, a real daemon, and real VMs. Its pure
/// lease-keys behavior is fully covered by the offline tests above; only the
/// live allocation/roundtrip/negative-fault portions remain, and they must not
/// run in CI.
#[test]
#[ignore = "no Python test counterpart: the live runner in tests/e2e/test_lease_keys_live.py is \
            a script (main(), no test_ functions) that needs a real VM, network, daemon and parent \
            authorization; its offline behavior is covered by the tests above"]
fn live_lease_keys_acceptance_requires_authorization() {}
