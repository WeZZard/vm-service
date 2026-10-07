//! TL-1 unit tests (`docs/arch/test-layers.md`): the agent's request loop on a
//! thread over OS pipes, and the client's GS-5 outcomes against scripted fake
//! agents. No guest, SSH or network is involved.

use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::agent::{hex, serve, AgentOptions, PROTOCOL};
use crate::client::{Op, Session, SessionError};
use crate::frame::{read_frame, write_frame, Frame, FrameError, Kind};

const OPEN: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(20);

fn options(home: &Path) -> AgentOptions {
    AgentOptions {
        shell: PathBuf::from("/bin/sh"),
        home: home.to_path_buf(),
        build: "test-build".to_string(),
        heartbeat: None,
    }
}

/// A real agent on a thread, reached through OS pipes.
fn agent(home: &Path) -> (PipeReader, PipeWriter, JoinHandle<Result<(), FrameError>>) {
    let (host_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, host_writer) = io::pipe().unwrap();
    let options = options(home);
    let handle = thread::spawn(move || serve(agent_reader, agent_writer, options));
    (host_reader, host_writer, handle)
}

fn session(home: &Path) -> (Session, JoinHandle<Result<(), FrameError>>) {
    let (reader, writer, handle) = agent(home);
    (Session::open(reader, writer, OPEN).unwrap(), handle)
}

/// Raw frames to and from a real agent, past its hello.
struct Raw {
    reader: PipeReader,
    writer: PipeWriter,
    handle: JoinHandle<Result<(), FrameError>>,
}

impl Raw {
    fn new(home: &Path) -> Raw {
        let (mut reader, writer, handle) = agent(home);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap().kind, Kind::Hello);
        Raw {
            reader,
            writer,
            handle,
        }
    }

    fn send(&mut self, frame: Frame) {
        write_frame(&mut self.writer, &frame).unwrap();
    }

    fn receive(&mut self) -> Frame {
        read_frame(&mut self.reader).unwrap().expect("a frame")
    }

    fn end(&mut self, id: u64, bytes: &[u8]) {
        let end = json!({"size": bytes.len(), "sha256": hex(&Sha256::digest(bytes))});
        self.send(Frame::json(Kind::End, id, &end));
    }

    /// Close the agent's input and wait for it to stop.
    fn close(self) -> Result<(), FrameError> {
        drop(self.writer);
        self.handle.join().unwrap()
    }
}

fn exec(session: &mut Session, command: &str) -> crate::client::ExecOutcome {
    session.exec(command, None, LONG).unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + LONG;
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if content.ends_with('\n') {
                return content;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        thread::sleep(Duration::from_millis(20));
    }
}

fn leftovers(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect()
}

// ------------------------------------------------------------- agent loop

#[test]
fn hello_names_protocol_build_and_os() {
    let home = tempfile::tempdir().unwrap();
    let (session, _agent) = session(home.path());
    assert_eq!(session.hello().protocol, PROTOCOL);
    assert_eq!(session.hello().build, "test-build");
    assert_eq!(session.hello().os, std::env::consts::OS);
}

#[test]
fn exec_returns_combined_output_and_the_exit_code() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let outcome = exec(&mut session, "printf 'a b\\n'; echo err >&2; exit 7");
    assert_eq!(outcome.rc, Some(7));
    assert_eq!(outcome.signal, None);
    assert!(!outcome.timed_out && !outcome.cancelled);
    let output = text(&outcome.output);
    assert!(
        output.contains("a b\n") && output.contains("err\n"),
        "{output:?}"
    );
}

/// GS-3: commands run through the shell in the home directory, as `sshd`
/// runs a remote command, whichever channel started the agent.
#[test]
fn exec_runs_through_the_shell_in_the_home_directory() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let outcome = exec(&mut session, "echo $0; pwd -P");
    // `sshd` names the shell by its file name, so `$0` and the shell's own
    // messages read "sh", not "/bin/sh".
    let expected = format!("sh\n{}\n", home.path().canonicalize().unwrap().display());
    assert_eq!(text(&outcome.output), expected);
}

#[test]
fn a_missing_command_exits_127() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    assert_eq!(
        exec(&mut session, "no-such-command-guest-session").rc,
        Some(127)
    );
}

#[test]
fn a_script_arrives_on_standard_input() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let script = b"echo from-stdin\nexit 3\n";
    let outcome = session.exec("/bin/sh -s", Some(script), LONG).unwrap();
    assert_eq!(
        (outcome.rc, text(&outcome.output)),
        (Some(3), "from-stdin\n".to_string())
    );
}

#[test]
fn a_command_without_stdin_reads_end_of_file() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let outcome = exec(&mut session, "cat; echo done");
    assert_eq!(text(&outcome.output), "done\n");
}

#[test]
fn large_output_arrives_whole() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let outcome = exec(&mut session, "head -c 3000000 /dev/zero | tr '\\0' x");
    assert_eq!(outcome.output.len(), 3_000_000);
    assert!(outcome.output.iter().all(|&byte| byte == b'x'));
}

#[test]
fn a_timeout_kills_the_whole_process_group() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let (mut session, _agent) = session(home.path());
    let started = Instant::now();
    let command = format!(
        "sh -c 'echo $$ > {}; exec sleep 30' & wait",
        pid_file.display()
    );
    let outcome = session
        .exec(&command, None, Duration::from_millis(500))
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(outcome.timed_out, "{outcome:?}");
    assert_eq!(outcome.signal, Some(libc::SIGKILL));
    let grandchild: i32 = wait_for(&pid_file).trim().parse().unwrap();
    wait_gone(grandchild);
}

#[test]
fn cancel_kills_a_running_command() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let flag = Arc::new(AtomicBool::new(false));
    let setter = {
        let flag = Arc::clone(&flag);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    session.cancel_on(Arc::clone(&flag));
    let outcome = session.exec("sleep 30", None, LONG).unwrap();
    setter.join().unwrap();
    assert!(outcome.cancelled, "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The session stays usable once the flag is lowered.
    flag.store(false, Ordering::SeqCst);
    assert_eq!(exec(&mut session, "echo after").output, b"after\n");
}

#[test]
fn ping_round_trips() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    assert!(session.ping(OPEN).unwrap() < OPEN);
}

#[test]
fn many_requests_share_one_session() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    for index in 0..50 {
        assert_eq!(
            text(&exec(&mut session, &format!("echo {index}")).output),
            format!("{index}\n")
        );
    }
}

/// GS-3: the commit point of an exec is its end frame. A command whose end
/// frame never arrived never ran.
#[test]
fn an_exec_without_its_end_frame_never_runs() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("marker");
    let mut raw = Raw::new(home.path());
    let command = format!("echo ran >> {}", marker.display());
    raw.send(Frame::json(
        Kind::Exec,
        1,
        &json!({"command": command, "stdin": false, "timeout_s": 10}),
    ));
    raw.close().unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
}

#[test]
fn standard_input_that_does_not_match_its_end_frame_never_runs() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("marker");
    let mut raw = Raw::new(home.path());
    raw.send(Frame::json(
        Kind::Exec,
        1,
        &json!({"command": "/bin/sh -s", "stdin": true, "timeout_s": 10}),
    ));
    let script = format!("echo ran >> {}\n", marker.display());
    raw.send(Frame::new(Kind::Data, 1, script.as_bytes().to_vec()));
    raw.end(1, b"something else");
    let reply = raw.receive();
    assert_eq!(
        (reply.kind, reply.value().unwrap()["code"].clone()),
        (Kind::Error, json!("checksum_mismatch"))
    );
    raw.close().unwrap();
    assert!(!marker.exists());
}

#[test]
fn the_end_of_input_kills_running_commands() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let mut raw = Raw::new(home.path());
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    raw.send(Frame::json(
        Kind::Exec,
        1,
        &json!({"command": command, "stdin": false, "timeout_s": 60}),
    ));
    raw.end(1, b"");
    let pid: i32 = wait_for(&pid_file).trim().parse().unwrap();
    assert!(alive(pid));
    raw.close().unwrap();
    wait_gone(pid);
}

#[test]
fn a_malformed_frame_ends_the_session_and_kills_running_commands() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let mut raw = Raw::new(home.path());
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    raw.send(Frame::json(
        Kind::Exec,
        1,
        &json!({"command": command, "stdin": false, "timeout_s": 60}),
    ));
    raw.end(1, b"");
    let pid: i32 = wait_for(&pid_file).trim().parse().unwrap();
    let mut corrupted = Frame::new(Kind::Ping, 2, vec![0; 8]).encode();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 1;
    raw.writer.write_all(&corrupted).unwrap();
    assert!(matches!(
        raw.handle.join().unwrap(),
        Err(FrameError::PayloadChecksum)
    ));
    wait_gone(pid);
}

#[test]
fn a_request_id_in_use_or_zero_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let mut raw = Raw::new(home.path());
    raw.send(Frame::json(
        Kind::Exec,
        0,
        &json!({"command": "true", "timeout_s": 1}),
    ));
    assert_eq!(raw.receive().value().unwrap()["code"], "duplicate_request");
    raw.send(Frame::json(
        Kind::Exec,
        5,
        &json!({"command": "true", "timeout_s": 1}),
    ));
    raw.send(Frame::json(
        Kind::Exec,
        5,
        &json!({"command": "true", "timeout_s": 1}),
    ));
    let reply = raw.receive();
    assert_eq!(
        (reply.request, reply.value().unwrap()["code"].clone()),
        (5, json!("duplicate_request"))
    );
    raw.close().unwrap();
}

#[test]
fn frames_the_agent_sends_are_refused_when_received() {
    let home = tempfile::tempdir().unwrap();
    let mut raw = Raw::new(home.path());
    raw.send(Frame::json(Kind::Exit, 3, &json!({})));
    assert_eq!(raw.receive().value().unwrap()["code"], "unexpected_frame");
    raw.send(Frame::new(Kind::Alive, 4, Vec::new()));
    let reply = raw.receive();
    assert_eq!(
        (
            reply.kind,
            reply.request,
            reply.value().unwrap()["code"].clone()
        ),
        (Kind::Error, 4, json!("unexpected_frame"))
    );
    raw.close().unwrap();
}

// ------------------------------------------------------------- files

#[test]
fn a_file_written_and_read_back_matches_with_its_mode() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let content: Vec<u8> = (0..700_000u32).map(|n| (n * 7 % 251) as u8).collect();
    let written = session
        .write(&mut &content[..], "out.bin", "ignored", Some(0o640), LONG)
        .unwrap();
    assert_eq!(written.size, content.len() as u64);
    assert_eq!(written.sha256, hex(&Sha256::digest(&content)));
    let path = home.path().join("out.bin");
    assert_eq!(std::fs::read(&path).unwrap(), content);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let mut back = Vec::new();
    let read = session.read("~/out.bin", &mut back, LONG).unwrap();
    assert_eq!((back, read), (content, written));
    assert!(leftovers(home.path()).is_empty());
}

#[test]
fn a_write_into_a_directory_uses_the_source_name() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("inbox")).unwrap();
    let (mut session, _agent) = session(home.path());
    session
        .write(&mut &b"hi"[..], "inbox", "note.txt", None, LONG)
        .unwrap();
    assert_eq!(
        std::fs::read(home.path().join("inbox/note.txt")).unwrap(),
        b"hi"
    );
    let error = session
        .write(&mut &b"hi"[..], "inbox", "../escape", None, LONG)
        .unwrap_err();
    assert!(
        matches!(error, SessionError::Remote { ref code, .. } if code == "invalid_request"),
        "{error}"
    );
}

#[test]
fn a_write_replaces_an_existing_file_whole() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("f"), b"old content that is longer").unwrap();
    let (mut session, _agent) = session(home.path());
    session
        .write(
            &mut &b"new"[..],
            &home.path().join("f").to_string_lossy(),
            "",
            None,
            LONG,
        )
        .unwrap();
    assert_eq!(std::fs::read(home.path().join("f")).unwrap(), b"new");
}

#[test]
fn transfer_errors_are_reported_and_leave_the_session_usable() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("dir")).unwrap();
    let (mut session, _agent) = session(home.path());
    let code = |error: SessionError| match error {
        SessionError::Remote { code, .. } => code,
        other => panic!("{other}"),
    };
    assert_eq!(
        code(session.read("dir", &mut Vec::new(), LONG).unwrap_err()),
        "is_directory"
    );
    assert_eq!(
        code(session.read("missing", &mut Vec::new(), LONG).unwrap_err()),
        "not_found"
    );
    let error = session
        .write(&mut &b"x"[..], "no/such/dir/f", "f", None, LONG)
        .unwrap_err();
    assert_eq!(code(error), "not_found");
    assert_eq!(exec(&mut session, "echo ok").output, b"ok\n");
}

#[test]
fn a_write_that_does_not_match_its_end_frame_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let mut raw = Raw::new(home.path());
    raw.send(Frame::json(
        Kind::Write,
        1,
        &json!({"path": "f", "name": "f"}),
    ));
    raw.send(Frame::new(Kind::Data, 1, b"abc".to_vec()));
    raw.end(1, b"abd");
    assert_eq!(raw.receive().value().unwrap()["code"], "checksum_mismatch");
    raw.close().unwrap();
    assert!(!home.path().join("f").exists());
    assert!(leftovers(home.path()).is_empty());
}

#[test]
fn a_write_cut_before_its_end_frame_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("f"), b"before").unwrap();
    let mut raw = Raw::new(home.path());
    raw.send(Frame::json(
        Kind::Write,
        1,
        &json!({"path": "f", "name": "f"}),
    ));
    raw.send(Frame::new(Kind::Data, 1, b"partial".to_vec()));
    raw.close().unwrap();
    assert_eq!(std::fs::read(home.path().join("f")).unwrap(), b"before");
    assert!(leftovers(home.path()).is_empty());
}

#[test]
fn read_to_path_leaves_nothing_when_the_read_fails() {
    let home = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let (mut session, _agent) = session(home.path());
    let destination = host.path().join("copy");
    assert!(session.read_to_path("missing", &destination, LONG).is_err());
    assert!(!destination.exists());
    assert!(leftovers(host.path()).is_empty());
    std::fs::write(home.path().join("present"), b"data").unwrap();
    session.read_to_path("present", host.path(), LONG).unwrap();
    assert_eq!(std::fs::read(host.path().join("present")).unwrap(), b"data");
}

/// A source of `total` 4 KiB reads that raises `flag` on read `raise_at`.
struct RaiseOnRead {
    reads: usize,
    raise_at: usize,
    total: usize,
    flag: Arc<AtomicBool>,
}

impl Read for RaiseOnRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.reads == self.total {
            return Ok(0);
        }
        self.reads += 1;
        if self.reads == self.raise_at {
            self.flag.store(true, Ordering::SeqCst);
        }
        let count = buffer.len().min(4096);
        buffer[..count].fill(7);
        Ok(count)
    }
}

/// A sink that raises `flag` on its first write.
struct RaiseOnWrite(Arc<AtomicBool>);

impl Write for RaiseOnWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.store(true, Ordering::SeqCst);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_write_cancelled_midway_closes_the_session_and_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, agent) = session(home.path());
    let flag = Arc::new(AtomicBool::new(false));
    session.cancel_on(Arc::clone(&flag));
    let mut source = RaiseOnRead {
        reads: 0,
        raise_at: 3,
        total: 100,
        flag,
    };
    let error = session
        .write(&mut source, "out", "out", None, LONG)
        .unwrap_err();
    assert!(
        matches!(error, SessionError::Cancelled { committed: false }),
        "{error:?}"
    );
    assert!(!error.may_resend(Op::Write) && !error.is_uncertain(Op::Write));
    // The session closed its input, so the agent stopped and removed the
    // unfinished file.
    agent.join().unwrap().unwrap();
    assert!(!home.path().join("out").exists());
    assert!(
        leftovers(home.path()).is_empty(),
        "{:?}",
        leftovers(home.path())
    );
}

#[test]
fn a_read_cancelled_midway_closes_the_session() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("big"), vec![1u8; 4 << 20]).unwrap();
    let (mut session, agent) = session(home.path());
    let flag = Arc::new(AtomicBool::new(false));
    session.cancel_on(Arc::clone(&flag));
    let error = session
        .read("big", &mut RaiseOnWrite(flag), LONG)
        .unwrap_err();
    assert!(matches!(error, SessionError::Cancelled { .. }), "{error:?}");
    assert!(!error.may_resend(Op::Read) && !error.is_uncertain(Op::Read));
    agent.join().unwrap().unwrap();
}

#[test]
fn a_raised_flag_refuses_every_new_request() {
    let home = tempfile::tempdir().unwrap();
    let (mut session, agent) = session(home.path());
    session.cancel_on(Arc::new(AtomicBool::new(true)));
    let error = session.exec("touch ran", None, LONG).unwrap_err();
    assert!(
        matches!(error, SessionError::Cancelled { committed: false }),
        "{error:?}"
    );
    assert!(!error.may_resend(Op::Exec) && !error.is_uncertain(Op::Exec));
    agent.join().unwrap().unwrap();
    assert!(!home.path().join("ran").exists());
}

// ------------------------------------------------------------- GS-5 outcomes

/// A scripted fake agent: sends a hello, then runs `script` on the raw
/// stream.
fn fake(script: impl FnOnce(PipeReader, PipeWriter) + Send + 'static) -> (Session, JoinHandle<()>) {
    fake_with(
        json!({"protocol": PROTOCOL, "build": "fake", "os": "fake"}),
        script,
    )
}

/// A scripted fake agent whose hello carries `hello`.
fn fake_with(
    hello: serde_json::Value,
    script: impl FnOnce(PipeReader, PipeWriter) + Send + 'static,
) -> (Session, JoinHandle<()>) {
    let (host_reader, mut agent_writer) = io::pipe().unwrap();
    let (agent_reader, host_writer) = io::pipe().unwrap();
    write_frame(&mut agent_writer, &Frame::json(Kind::Hello, 0, &hello)).unwrap();
    let handle = thread::spawn(move || script(agent_reader, agent_writer));
    (
        Session::open(host_reader, host_writer, OPEN).unwrap(),
        handle,
    )
}

/// Read frames until an end frame for `id`.
fn until_end(reader: &mut PipeReader, id: u64) {
    while let Some(frame) = read_frame(reader).unwrap() {
        if frame.kind == Kind::End && frame.request == id {
            return;
        }
    }
    panic!("no end frame");
}

#[test]
fn an_exec_lost_after_its_end_frame_is_uncertain() {
    let (mut session, script) = fake(|mut reader, writer| {
        until_end(&mut reader, 1);
        drop((reader, writer));
    });
    let error = session.exec("true", None, LONG).unwrap_err();
    script.join().unwrap();
    assert!(
        matches!(
            error,
            SessionError::Lost {
                committed: true,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.is_uncertain(Op::Exec));
    assert!(!error.may_resend(Op::Exec));
    // The broken session refuses further requests without sending them.
    let next = session.exec("true", None, LONG).unwrap_err();
    assert!(matches!(
        next,
        SessionError::Lost {
            committed: false,
            ..
        }
    ));
}

#[test]
fn a_session_known_lost_before_sending_is_not_committed() {
    let (mut session, script) = fake(|reader, writer| drop((reader, writer)));
    script.join().unwrap();
    // Let the reader thread see the end of the stream.
    thread::sleep(Duration::from_millis(100));
    let error = session.exec("true", None, LONG).unwrap_err();
    assert!(
        matches!(
            error,
            SessionError::Lost {
                committed: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.may_resend(Op::Exec));
    assert!(!error.is_uncertain(Op::Exec));
}

#[test]
fn a_write_lost_before_its_end_frame_may_be_resent() {
    let (mut session, script) = fake(|mut reader, writer| {
        let first = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(first.kind, Kind::Write);
        drop((reader, writer));
    });
    let content = vec![1u8; 8 << 20];
    let error = session
        .write(&mut &content[..], "f", "f", None, LONG)
        .unwrap_err();
    script.join().unwrap();
    assert!(
        matches!(
            error,
            SessionError::Lost {
                committed: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.may_resend(Op::Write));
}

#[test]
fn a_write_lost_after_its_end_frame_is_uncertain() {
    let (mut session, script) = fake(|mut reader, writer| {
        until_end(&mut reader, 1);
        drop((reader, writer));
    });
    let error = session
        .write(&mut &b"x"[..], "f", "f", None, LONG)
        .unwrap_err();
    script.join().unwrap();
    assert!(
        error.is_uncertain(Op::Write) && !error.may_resend(Op::Write),
        "{error}"
    );
}

#[test]
fn a_read_lost_midway_may_be_resent() {
    let (mut session, script) = fake(|mut reader, mut writer| {
        let request = read_frame(&mut reader).unwrap().unwrap();
        write_frame(
            &mut writer,
            &Frame::new(Kind::Data, request.request, vec![0; 1000]),
        )
        .unwrap();
        drop((reader, writer));
    });
    let error = session.read("f", &mut Vec::new(), LONG).unwrap_err();
    script.join().unwrap();
    assert!(
        matches!(
            error,
            SessionError::Lost {
                committed: true,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.may_resend(Op::Read) && !error.is_uncertain(Op::Read));
}

#[test]
fn a_corrupt_frame_after_the_commit_point_is_uncertain() {
    let (mut session, script) = fake(|mut reader, mut writer| {
        until_end(&mut reader, 1);
        let mut bytes = Frame::new(Kind::Output, 1, b"hello".to_vec()).encode();
        bytes[22] ^= 0x10;
        writer.write_all(&bytes).unwrap();
        // Hold the stream open: the corruption alone must end the request.
        let _ = reader.read(&mut [0u8; 1]);
    });
    let error = session.exec("true", None, LONG).unwrap_err();
    assert!(error.is_uncertain(Op::Exec), "{error}");
    assert!(error.to_string().contains("checksum"), "{error}");
    // Closing the broken session lets the fake agent finish.
    session.close();
    script.join().unwrap();
}

#[test]
fn a_read_whose_bytes_do_not_match_their_end_frame_fails() {
    let (mut session, script) = fake(|mut reader, mut writer| {
        let request = read_frame(&mut reader).unwrap().unwrap();
        write_frame(
            &mut writer,
            &Frame::new(Kind::Data, request.request, b"abc".to_vec()),
        )
        .unwrap();
        let end = json!({"size": 3, "sha256": hex(&Sha256::digest(b"abd"))});
        write_frame(&mut writer, &Frame::json(Kind::End, request.request, &end)).unwrap();
        let _ = reader.read(&mut [0u8; 1]);
    });
    let error = session.read("f", &mut Vec::new(), LONG).unwrap_err();
    assert!(
        matches!(error, SessionError::Lost { .. }) && error.may_resend(Op::Read),
        "{error}"
    );
    session.close();
    script.join().unwrap();
}

#[test]
fn an_agent_that_never_answers_fails_at_the_deadline() {
    let (mut session, script) = fake(|mut reader, writer| {
        let _ = std::io::copy(&mut reader, &mut io::sink());
        drop(writer);
    });
    let started = Instant::now();
    let error = session.ping(Duration::from_millis(300)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(error, SessionError::Lost { .. }), "{error}");
    session.close();
    script.join().unwrap();
}

#[test]
fn no_hello_means_no_session() {
    let (host_reader, agent_writer) = io::pipe().unwrap();
    let (_agent_reader, host_writer) = io::pipe().unwrap();
    let error = Session::open(host_reader, host_writer, Duration::from_millis(200))
        .err()
        .expect("no session");
    assert!(matches!(
        error,
        SessionError::Lost {
            committed: false,
            ..
        }
    ));
    drop(agent_writer);
}

#[test]
fn an_agent_that_ignores_a_cancel_is_given_up_on_after_the_grace() {
    let (mut session, _fake) = fake(|mut reader, _writer| {
        // Read the exec and everything after it, answering nothing.
        while let Ok(Some(_)) = read_frame(&mut reader) {}
    });
    let flag = Arc::new(AtomicBool::new(false));
    session.cancel_on(Arc::clone(&flag));
    let raiser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let error = session.exec("sleep 600", None, LONG).unwrap_err();
    raiser.join().unwrap();
    assert!(
        matches!(error, SessionError::Lost { committed: true, ref reason } if reason.contains("did not stop")),
        "{error:?}"
    );
    assert!(error.is_uncertain(Op::Exec));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= crate::client::CANCEL_GRACE && elapsed < LONG,
        "{elapsed:?}"
    );
}

// ------------------------------------------------------------- GS-1 heartbeat

const BEAT: Duration = Duration::from_millis(50);

/// A fake agent that announces alive frames every [`BEAT`] but sends none.
fn mute(script: impl FnOnce(PipeReader, PipeWriter) + Send + 'static) -> (Session, JoinHandle<()>) {
    fake_with(
        json!({"protocol": PROTOCOL, "build": "fake", "os": "fake", "heartbeat_ms": 50}),
        script,
    )
}

/// Read and discard the host's frames until it closes its side.
fn swallow(mut reader: PipeReader) {
    let _ = std::io::copy(&mut reader, &mut io::sink());
}

#[test]
fn the_agent_announces_its_heartbeat_and_keeps_to_it() {
    let home = tempfile::tempdir().unwrap();
    let (host_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, host_writer) = io::pipe().unwrap();
    let mut options = options(home.path());
    options.heartbeat = Some(BEAT);
    let agent = thread::spawn(move || serve(agent_reader, agent_writer, options));
    let mut reader = host_reader;
    let hello = read_frame(&mut reader).unwrap().unwrap();
    assert_eq!(hello.kind, Kind::Hello);
    assert_eq!(hello.value().unwrap()["heartbeat_ms"], 50);
    let started = Instant::now();
    for _ in 0..3 {
        let frame = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(
            (frame.kind, frame.request, frame.payload.len()),
            (Kind::Alive, 0, 0)
        );
    }
    assert!(started.elapsed() >= BEAT * 2, "{:?}", started.elapsed());
    drop(host_writer);
    agent.join().unwrap().unwrap();
}

#[test]
fn an_agent_without_a_heartbeat_announces_none_and_is_never_silent() {
    let home = tempfile::tempdir().unwrap();
    let (session, _agent) = session(home.path());
    assert_eq!(session.silence_limit(), None);
}

#[test]
fn alive_frames_keep_an_idle_session_open() {
    let home = tempfile::tempdir().unwrap();
    let (host_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, host_writer) = io::pipe().unwrap();
    let mut options = options(home.path());
    options.heartbeat = Some(BEAT);
    let _agent = thread::spawn(move || serve(agent_reader, agent_writer, options));
    let mut session = Session::open(host_reader, host_writer, OPEN).unwrap();
    let limit = session.silence_limit().expect("a limit");
    assert_eq!(limit, BEAT * crate::client::MISSED_HEARTBEATS);
    let fired = Arc::new(AtomicBool::new(false));
    let hook = Arc::clone(&fired);
    session.on_silence(move || hook.store(true, Ordering::SeqCst));
    thread::sleep(limit * 3);
    let outcome = exec(&mut session, "echo still-here");
    assert_eq!(text(&outcome.output), "still-here\n");
    assert!(!fired.load(Ordering::SeqCst), "the watchdog fired");
}

#[test]
fn a_silent_agent_makes_a_committed_exec_uncertain_within_the_limit() {
    let (mut session, _script) = mute(|mut reader, writer| {
        // Take the request, then say nothing, as across a dead network.
        until_end(&mut reader, 1);
        swallow(reader);
        drop(writer);
    });
    let limit = session.silence_limit().expect("a limit");
    let started = Instant::now();
    let error = session.exec("true", None, LONG).unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(error, SessionError::Lost { committed: true, ref reason } if reason.contains("nothing from the agent")),
        "{error:?}"
    );
    assert!(error.is_uncertain(Op::Exec));
    assert!(!error.may_resend(Op::Exec));
    assert!(
        elapsed >= limit / 2 && elapsed < limit + Duration::from_secs(1),
        "{elapsed:?}"
    );
}

#[test]
fn a_session_silent_before_a_request_refuses_it_uncommitted() {
    let (mut session, _script) = mute(|reader, writer| {
        swallow(reader);
        drop(writer);
    });
    let limit = session.silence_limit().expect("a limit");
    thread::sleep(limit + BEAT);
    let error = session.exec("true", None, LONG).unwrap_err();
    assert!(
        matches!(error, SessionError::Lost { committed: false, ref reason } if reason.contains("nothing from the agent")),
        "{error:?}"
    );
    assert!(error.may_resend(Op::Exec));
}

#[test]
fn the_watchdog_ends_a_silent_channel_once() {
    let (mut session, _script) = mute(|reader, writer| {
        swallow(reader);
        drop(writer);
    });
    let limit = session.silence_limit().expect("a limit");
    let (fired, calls) = std::sync::mpsc::channel();
    let started = Instant::now();
    session.on_silence(move || fired.send(Instant::now()).unwrap());
    let at = calls
        .recv_timeout(Duration::from_secs(5))
        .expect("the watchdog fired");
    assert!(at - started >= limit, "{:?}", at - started);
    assert!(calls.recv_timeout(limit * 2).is_err(), "fired twice");
}

#[test]
fn the_watchdog_stops_when_the_session_closes() {
    let (mut session, _script) = mute(|reader, writer| {
        swallow(reader);
        drop(writer);
    });
    let limit = session.silence_limit().expect("a limit");
    let (fired, calls) = std::sync::mpsc::channel::<()>();
    session.on_silence(move || {
        let _ = fired.send(());
    });
    session.close();
    assert!(calls.recv_timeout(limit * 3).is_err(), "fired after close");
}

#[test]
fn a_frame_still_crossing_a_slow_link_is_not_silence() {
    let (mut session, _script) = mute(|mut reader, mut writer| {
        let ping = read_frame(&mut reader).unwrap().unwrap();
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Frame::new(Kind::Pong, ping.request, ping.payload),
        )
        .unwrap();
        // One byte per beat: the whole frame takes longer than the limit,
        // but bytes keep arriving.
        for byte in bytes {
            writer.write_all(&[byte]).unwrap();
            thread::sleep(BEAT);
        }
        swallow(reader);
    });
    let limit = session.silence_limit().expect("a limit");
    let started = Instant::now();
    session.ping(LONG).expect("pong");
    assert!(started.elapsed() > limit, "{:?}", started.elapsed());
}
