//! Lease lifecycle reproducers at the daemon process boundary.
//!
//! These tests start the real `vm-service` binary against the PATH shims in
//! `tests/common/mod.rs`, with an isolated temporary state directory and Tart
//! store. They cover the lifecycle failures that exist only in the daemon
//! entry point (startup, HTTP response delivery). See
//! `docs/lifecycle-fixes.md`.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::LegacyFixture;
use serde_json::{json, Value};

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs_f64()
}

/// A lease record as a previous daemon process would have persisted it.
fn record(vm: &str, purpose: &str, image: &str, kind: &str, state: &str) -> Value {
    let now = unix_now();
    json!({
        "vm": vm,
        "purpose": purpose,
        "image": image,
        "image_kind": kind,
        "lease_id": "0123456789abcdef0123456789abcdef",
        "env": null,
        "state": state,
        "created_at": now - 60.0,
        "ttl_expires_at": now + 24.0 * 3600.0,
        "grace_until": null,
        "warned": false,
        "ip": null,
        "cpu": null,
        "memory_mb": null,
        "disk_gb": null,
        "ssh_auth": "lease-key",
        "ssh_user": "admin",
        "ssh_verified": state == "running",
        "ssh_host_trust": "lease-tofu",
    })
}

/// V1: a daemon restart during acquisition leaves `pending`/`provisioning`
/// records, a clone, and a running `tart run`. They held capacity until
/// TTL + grace and blocked a retry with the same purpose. Startup must
/// release them through the normal teardown path before serving requests,
/// and must leave `running` leases alone.
#[test]
fn startup_reconciles_acquisitions_interrupted_by_a_restart() {
    let fixture = LegacyFixture::new();
    let pending = "pilot-interrupted-a1b2c3";
    let provisioning = "pilot-mac-interrupted-d4e5f6";
    let running = "pilot-kept-0a0b0c";
    let state = json!({
        "vms": {
            pending: record(pending, "interrupted", "ubuntu2404", "linux", "pending"),
            provisioning: record(provisioning, "interrupted", "macos26", "macos", "provisioning"),
            running: record(running, "kept", "ubuntu2404", "linux", "running"),
        }
    });
    std::fs::write(
        fixture.state_file(),
        serde_json::to_vec_pretty(&state).expect("state json"),
    )
    .expect("write state");
    // The pending clone was never created; the provisioning clone was booted
    // by the previous process and is still running.
    std::fs::write(
        fixture.tart_home.join("fake-vms"),
        format!("pilot-ubuntu-work=0\n{provisioning}=1\n{running}=1\n"),
    )
    .expect("fake-vms");

    let _daemon = fixture.start_daemon();

    let vms = fixture.read_state();
    assert!(
        !vms.contains_key(provisioning),
        "a provisioning record from the previous process survived startup: {vms:?}"
    );
    assert!(
        !vms.contains_key(pending),
        "a pending record from the previous process survived startup: {vms:?}"
    );
    assert!(vms.contains_key(running), "a running lease was reclaimed");
    let calls = fixture.tart_calls();
    assert!(
        calls.contains(&format!("stop {provisioning}")),
        "the orphaned clone was not stopped: {calls}"
    );
    assert!(
        calls.contains(&format!("delete {provisioning}")),
        "the orphaned clone was not deleted: {calls}"
    );
    let log = std::fs::read_to_string(fixture.state_dir.join("service.log")).unwrap_or_default();
    assert!(
        log.contains(&format!("startup: reconciling {provisioning}")),
        "reconciliation was not logged: {log}"
    );
}

/// Reset a TCP connection instead of closing it gracefully, so the daemon's
/// next write on it fails, as it does when a client times out and its socket
/// is torn down.
fn reset(stream: std::net::TcpStream) {
    use std::os::fd::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `setsockopt` reads `linger` for the duration of the call on a
    // descriptor owned by `stream`.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            std::ptr::addr_of!(linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "setsockopt(SO_LINGER)");
    drop(stream);
}

/// V3: an acquisition that completes after its client has gone must not leave
/// a lease that nobody knows about. The daemon ignored the result of writing
/// the response, so the new lease held its slot until TTL + grace.
#[test]
fn undelivered_acquire_response_releases_the_new_lease() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();

    let body = json!({"purpose": "abandoned", "image": "ubuntu2404", "env": "none"}).to_string();
    let request = format!(
        "POST /acquire HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
        port = fixture.port(),
    );
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port())).expect("connect");
    stream.write_all(request.as_bytes()).expect("send acquire");
    stream.flush().expect("flush");

    let lease = |state: &serde_json::Map<String, Value>| {
        state
            .values()
            .find(|record| record["purpose"] == json!("abandoned"))
            .cloned()
    };
    // Wait until the daemon has read the request and reserved the lease, then
    // give up on the response the way a timed-out client does.
    let deadline = Instant::now() + Duration::from_secs(20);
    let vm = loop {
        if let Some(record) = std::fs::read_to_string(fixture.state_file())
            .ok()
            .and_then(|_| lease(&fixture.read_state()))
        {
            break record["vm"].as_str().expect("vm").to_string();
        }
        assert!(
            Instant::now() < deadline,
            "acquisition never reserved a lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    reset(stream);

    // The acquisition itself still completes (about 10 s of boot settle).
    // Once it has, the lease must not outlive the failed response.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut running_since: Option<Instant> = None;
    loop {
        let state = fixture.read_state();
        match state.get(&vm) {
            None => break,
            Some(record) if record["state"] == json!("running") => {
                let since = *running_since.get_or_insert_with(Instant::now);
                assert!(
                    since.elapsed() < Duration::from_secs(5),
                    "the lease of an undelivered acquisition is still held: {record}"
                );
            }
            Some(_) => {}
        }
        assert!(Instant::now() < deadline, "acquisition did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
    let calls = fixture.tart_calls();
    assert!(
        calls.contains(&format!("delete {vm}")),
        "the clone of the undelivered lease was not deleted: {calls}"
    );
    let log = std::fs::read_to_string(fixture.state_dir.join("service.log")).unwrap_or_default();
    assert!(
        log.contains(&format!("acquire response for {vm} undelivered")),
        "the rollback was not logged: {log}"
    );
}

/// Acquire a lease, start a guest operation whose subprocess sleeps far
/// longer than the test (the `fixture-sleep` shims), release the lease while
/// it runs, and return the operation's response and the VM name.
fn release_during(route: &str, body: Value) -> (common::Response, String) {
    use std::time::{Duration, Instant};

    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    let response = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "preempted", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    let vm = response.body["vm"].as_str().expect("vm").to_string();

    let port = fixture.port();
    let path = format!("/vms/{vm}/{route}");
    let operation = std::thread::spawn(move || {
        common::http_timeout(
            port,
            "POST",
            &path,
            Some(&body),
            None,
            Duration::from_secs(90),
        )
    });
    let log = if route == "exec" {
        "ssh-calls.log"
    } else {
        "scp-calls.log"
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(fixture.tart_home.join(log))
        .unwrap_or_default()
        .contains("fixture-sleep")
    {
        assert!(
            Instant::now() < deadline,
            "the guest operation never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    let released = common::try_http_timeout(
        port,
        "POST",
        &format!("/vms/{vm}/release"),
        Some(&json!({})),
        None,
        Duration::from_secs(10),
    );
    let elapsed = started.elapsed();
    let released = released.unwrap_or_else(|error| {
        panic!("release did not answer within 10 s while {route} was running: {error}")
    });
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(
        elapsed < Duration::from_secs(2),
        "release waited {elapsed:?} for the running {route}"
    );
    let outcome = operation.join().expect("operation thread");
    assert!(
        !fixture.read_state().contains_key(&vm),
        "the released lease is still recorded"
    );
    let calls = fixture.tart_calls();
    assert!(
        calls.contains(&format!("delete {vm}")),
        "the released clone was not deleted: {calls}"
    );
    (outcome, vm)
}

/// V4: a release must preempt a running guest command. The per-VM operation
/// lock was held for the whole SSH call and release waited on it, so a long
/// command blocked its own release (and GC) for as long as it ran.
#[test]
fn release_preempts_a_running_exec() {
    let (outcome, vm) = release_during("exec", json!({"argv": ["fixture-sleep"], "timeout": 3600}));
    assert_eq!(outcome.status, 409, "{}", outcome.body);
    let error = outcome.body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("command cancelled by release of {vm}")),
        "{error}"
    );
}

/// V4: a release must preempt a running push, which holds the same lock.
#[test]
fn release_preempts_a_running_push() {
    let source = tempfile::NamedTempFile::new().expect("push source");
    let (outcome, vm) = release_during(
        "push",
        json!({"local_path": source.path(), "remote_path": "/tmp/fixture-sleep"}),
    );
    assert_eq!(outcome.status, 409, "{}", outcome.body);
    let error = outcome.body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("transfer cancelled by release of {vm}")),
        "{error}"
    );
}

/// V4: a release must preempt a running pull, which holds the same lock.
#[test]
fn release_preempts_a_running_pull() {
    let destination = tempfile::tempdir().expect("pull destination");
    let (outcome, vm) = release_during(
        "pull",
        json!({
            "local_path": destination.path().join("pulled"),
            "remote_path": "/tmp/fixture-sleep",
        }),
    );
    assert_eq!(outcome.status, 409, "{}", outcome.body);
    let error = outcome.body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("transfer cancelled by release of {vm}")),
        "{error}"
    );
}
