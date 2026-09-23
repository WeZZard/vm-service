//! Subprocess helpers that restore Python `subprocess.run(timeout=)` semantics.
//!
//! Rust has no timeout on `Child::wait`, so the helper polls and kills a child
//! that exceeds its deadline. Output and input are drained on separate threads
//! so a full pipe buffer can never deadlock a command.

use std::io::Read;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A fake [`run_capture`] execution: given the command, optional stdin, and
/// timeout it would have used, returns the outcome to report instead of
/// spawning a real child.
#[cfg(debug_assertions)]
type RunCaptureRunner =
    Box<dyn FnMut(&mut Command, Option<Vec<u8>>, Duration) -> Result<Output, ProcError>>;

// Test-only subprocess injection. Python patched `subprocess.run` to raise
// `TimeoutExpired`; integration tests link this crate without `cfg(test)`, so
// the seam is gated on `debug_assertions` instead: it exists in every dev and
// test build, defaults to real execution, and is compiled out entirely in
// release. The override is per-thread so parallel tests never observe each
// other's runner.
#[cfg(debug_assertions)]
thread_local! {
    static RUN_CAPTURE_OVERRIDE: std::cell::RefCell<Option<RunCaptureRunner>> =
        const { std::cell::RefCell::new(None) };
    static RUN_CAPTURE_INVOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Install or clear a per-thread replacement for [`run_capture`]'s execution.
///
/// With an override installed the child is never spawned; the closure's result
/// is returned verbatim. This exists only in `debug_assertions` builds.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn set_run_capture_override(runner: Option<RunCaptureRunner>) {
    RUN_CAPTURE_OVERRIDE.with(|cell| *cell.borrow_mut() = runner);
}

/// The number of [`run_capture`] invocations made on this thread.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn run_capture_invocations() -> usize {
    RUN_CAPTURE_INVOCATIONS.with(|cell| cell.get())
}

/// Reset this thread's [`run_capture_invocations`] counter to zero.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_run_capture_invocations() {
    RUN_CAPTURE_INVOCATIONS.with(|cell| cell.set(0));
}

/// An error from a timed subprocess invocation.
#[derive(Debug)]
pub enum ProcError {
    /// The process could not be spawned or its pipes failed.
    Io(std::io::Error),
    /// The process exceeded its deadline and was killed.
    Timeout,
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcError::Io(error) => write!(formatter, "{error}"),
            ProcError::Timeout => write!(formatter, "process timed out"),
        }
    }
}

impl std::error::Error for ProcError {}

/// Run a command to completion, capturing output and optionally feeding stdin.
///
/// The child is killed when `timeout` elapses. The returned `Output` is
/// equivalent to Python `subprocess.run(..., capture_output=True)`.
pub fn run_capture(
    command: &mut Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Output, ProcError> {
    #[cfg(debug_assertions)]
    {
        RUN_CAPTURE_INVOCATIONS.with(|cell| cell.set(cell.get() + 1));
        let overridden = RUN_CAPTURE_OVERRIDE.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let runner = borrow.as_mut()?;
            Some(runner(command, stdin.clone(), timeout))
        });
        if let Some(result) = overridden {
            return result;
        }
    }
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ProcError::Io)?;

    let writer = stdin.map(|input| {
        let mut handle = child.stdin.take().expect("stdin was piped");
        std::thread::spawn(move || {
            let _ = handle.write_all(&input);
            let _ = handle.flush();
            // Dropping the handle closes the pipe, signalling EOF.
        })
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(ProcError::Io)? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.map(|handle| handle.join());
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(ProcError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    if let Some(handle) = writer {
        let _ = handle.join();
    }
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Combine captured stdout and stderr the way the Python helpers do.
pub fn combined_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Convert a captured output into a `(returncode, combined_output)` pair.
pub fn code_and_text(output: &Output) -> (i32, String) {
    (output.status.code().unwrap_or(-1), combined_text(output))
}
