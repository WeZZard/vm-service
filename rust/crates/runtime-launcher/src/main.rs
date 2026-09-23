//! Service-owned guest launcher (port of `bin/runtime-launcher.c`).
//!
//! Build/stage during separately reviewed provisioning. The launcher runs only
//! as service root with no arguments, verifies that the `vmruntime` account is
//! isolated and that the staging tree is root-owned and immutable, drops to that
//! account, and `exec`s the bundled Node adapter with a fixed environment.
//!
//! It accepts no sudo, shell, password, user-controlled argv/environment, or
//! guest JSON proof. The host must verify this binary and the complete immutable
//! dependency manifest over the lease channel before use, plus independently
//! attest the process credentials. It does NOT create or lock accounts, so it
//! must never be used until clone provisioning is accepted.
//!
//! Every failure prints `runtime-launcher: <reason>` and exits `73`, matching
//! the C program.

use std::ffi::{CStr, CString};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;

/// Staging root, resolved through `/private` on macOS.
const ROOT: &str = "/var/tmp/recorder-rpc";
/// Exit code used by the C launcher for every refusal.
const EXIT_REFUSED: i32 = 73;

fn fail(reason: &str) -> ! {
    eprintln!("runtime-launcher: {reason}");
    std::process::exit(EXIT_REFUSED);
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    // SAFETY: `getuid`/`geteuid` take no arguments and only read process state.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if arguments.len() != 1 || uid != 0 || euid != 0 {
        fail("requires service root launch, no arguments");
    }

    let (runtime_uid, runtime_gid) = isolated_account();

    // /var aliases /private/var on macOS; validate the canonical ancestors.
    let resolved = match std::fs::canonicalize(ROOT) {
        Ok(path) => path,
        Err(_) => fail("missing staging root"),
    };
    let resolved_text = resolved.to_string_lossy().into_owned();
    if resolved_text != ROOT && resolved_text != format!("/private{ROOT}") {
        fail("aliased staging root");
    }
    immutable(&resolved_text, true);
    immutable(&format!("{ROOT}/code"), true);
    immutable(&format!("{ROOT}/code/node"), false);
    immutable(&format!("{ROOT}/code/adapter.mjs"), false);
    immutable(&format!("{ROOT}/code/runtime.json"), false);

    let output = match std::fs::symlink_metadata(format!("{ROOT}/output")) {
        Ok(metadata) => metadata,
        Err(_) => fail("unsafe output directory"),
    };
    if !output.is_dir()
        || output.uid() != runtime_uid
        || output.gid() != runtime_gid
        || output.mode() & 0o077 != 0
    {
        fail("unsafe output directory");
    }

    drop_credentials(runtime_uid, runtime_gid);
    verify_credentials(runtime_uid, runtime_gid);

    // Root identity must be unrecoverable once the credentials are dropped.
    // SAFETY: `setuid(0)` only attempts to change the process credentials; the
    // return value and errno are inspected immediately.
    let result = unsafe { libc::setuid(0) };
    if result == 0 {
        fail("root identity recoverable");
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        fail("root identity recoverable");
    }

    if std::env::set_current_dir(format!("{ROOT}/output")).is_err() {
        fail("cannot enter output directory");
    }
    // SAFETY: `umask` only sets the process file-creation mask.
    unsafe {
        libc::umask(0o077);
    }
    close_inherited_descriptors();

    // The adapter inherits only the service SSH standard streams.
    let error = std::process::Command::new(format!("{ROOT}/code/node"))
        .arg(format!("{ROOT}/code/adapter.mjs"))
        .arg("--config")
        .arg(format!("{ROOT}/code/runtime.json"))
        .env_clear()
        .env("HOME", format!("{ROOT}/output"))
        .env("TMPDIR", format!("{ROOT}/output"))
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .exec();
    let _ = error;
    fail("exec failed");
}

/// Look up the `vmruntime` account and confirm it is isolated.
fn isolated_account() -> (libc::uid_t, libc::gid_t) {
    let name = CString::new("vmruntime").expect("static account name has no NUL");
    // SAFETY: `getpwnam` returns a pointer into static storage that is read
    // immediately and never retained.
    let account = unsafe { libc::getpwnam(name.as_ptr()) };
    if account.is_null() {
        fail("runtime account not isolated");
    }
    let uid = unsafe { (*account).pw_uid };
    let gid = unsafe { (*account).pw_gid };
    let home = unsafe { (*account).pw_dir };
    if uid < 1000 || gid < 1000 || uid == 65534 {
        fail("runtime account not isolated");
    }
    let home_ok = !home.is_null()
        && unsafe { CStr::from_ptr(home) }.to_bytes() == format!("{ROOT}/output").as_bytes();
    if !home_ok {
        fail("runtime account not isolated");
    }

    // SAFETY: `getgrgid` returns a pointer into static storage; only `gr_name`
    // is read and compared.
    let group = unsafe { libc::getgrgid(gid) };
    if group.is_null() {
        fail("wrong primary group");
    }
    let group_name = unsafe { (*group).gr_name };
    let group_ok =
        !group_name.is_null() && unsafe { CStr::from_ptr(group_name) }.to_bytes() == b"vmruntime";
    if !group_ok {
        fail("wrong primary group");
    }

    // Check every group, including supplementary privilege grants.
    // SAFETY: the group database is walked and reset on every exit path below.
    unsafe {
        libc::setgrent();
        loop {
            let group = libc::getgrent();
            if group.is_null() {
                break;
            }
            let mut member = (*group).gr_mem;
            if !member.is_null() {
                loop {
                    let current = *member;
                    if current.is_null() {
                        break;
                    }
                    if CStr::from_ptr(current).to_bytes() == b"vmruntime" && (*group).gr_gid != gid
                    {
                        libc::endgrent();
                        fail("runtime has supplementary membership");
                    }
                    member = member.add(1);
                }
            }
        }
        libc::endgrent();
    }
    (uid, gid)
}

/// `lstat`-check that a path is root-owned, not writable by group or other, and
/// of the expected kind (`dir` when true, otherwise a single-link regular file).
fn immutable(path: &str, directory: bool) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => fail("unsafe immutable path"),
    };
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        fail("unsafe immutable path");
    }
    if directory {
        if !metadata.is_dir() {
            fail("unsafe immutable path");
        }
    } else if !metadata.is_file() || metadata.nlink() != 1 {
        fail("unsafe immutable path");
    }
}

fn drop_credentials(uid: libc::uid_t, gid: libc::gid_t) {
    let groups = [gid];
    // SAFETY: `setgroups`, `setgid` and `setuid` only change process credentials;
    // the return values are checked immediately.
    unsafe {
        if libc::setgroups(1, groups.as_ptr()) != 0 {
            fail("credential drop failed");
        }
        if libc::setgid(gid) != 0 {
            fail("credential drop failed");
        }
        if libc::setuid(uid) != 0 {
            fail("credential drop failed");
        }
    }
}

fn verify_credentials(uid: libc::uid_t, gid: libc::gid_t) {
    let mut groups = [0 as libc::gid_t; 2];
    // SAFETY: `getgroups` writes at most the two entries the buffer holds.
    let count = unsafe { libc::getgroups(2, groups.as_mut_ptr()) };
    // SAFETY: the getters take no arguments and only read process state.
    let (real_uid, effective_uid, real_gid, effective_gid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if real_uid != uid
        || effective_uid != uid
        || real_gid != gid
        || effective_gid != gid
        || count != 1
        || groups[0] != gid
    {
        fail("kernel credential verification failed");
    }
}

fn close_inherited_descriptors() {
    // No inherited descriptors except the service SSH stdin/stdout/stderr.
    // SAFETY: `sysconf` only queries the configured limit.
    let maximum = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if maximum < 0 {
        fail("cannot bound descriptor cleanup");
    }
    let mut descriptor = 3;
    while descriptor < maximum {
        // SAFETY: closing an unused descriptor is harmless.
        unsafe {
            libc::close(descriptor as libc::c_int);
        }
        descriptor += 1;
    }
}
