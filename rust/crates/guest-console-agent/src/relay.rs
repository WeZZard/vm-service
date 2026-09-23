//! Bounded nonblocking RFB relay. Mirrors the `relay` function of
//! `bin/guest-console-agent.py`.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::signal::check_interrupted;
use crate::Unavailable;

/// Per-direction buffer bound.
pub const RELAY_LIMIT: usize = 256 * 1024;
/// Largest single read or write request.
const CHUNK: usize = 65536;
/// Default periodic session/isolation re-check interval.
const DEFAULT_CHECK_INTERVAL_MILLIS: u64 = 2000;

static CHECK_INTERVAL_MILLIS: AtomicU64 = AtomicU64::new(DEFAULT_CHECK_INTERVAL_MILLIS);

/// The periodic re-check interval.
pub fn check_interval() -> Duration {
    Duration::from_millis(CHECK_INTERVAL_MILLIS.load(Ordering::SeqCst))
}

/// Override the re-check interval for tests.
#[cfg(test)]
pub fn set_check_interval(interval: Duration) {
    CHECK_INTERVAL_MILLIS.store(interval.as_millis() as u64, Ordering::SeqCst);
}

#[cfg(test)]
pub fn reset_check_interval() {
    CHECK_INTERVAL_MILLIS.store(DEFAULT_CHECK_INTERVAL_MILLIS, Ordering::SeqCst);
}

/// Outcome of a nonblocking read.
enum Read {
    Bytes(Vec<u8>),
    WouldBlock,
    Closed,
    Failed,
}

/// Outcome of a nonblocking write.
enum Write {
    Count(usize),
    WouldBlock,
    Closed,
    Failed,
}

/// Relay bytes between `(input_fd, output_fd)` and `endpoint`.
///
/// EOF on either side revokes the whole stream. `check` is run every
/// [`check_interval`] and `deadline` bounds the whole transfer.
pub fn relay<E: AsRawFd>(
    endpoint: &E,
    deadline: Instant,
    check: &mut dyn FnMut() -> Result<(), Unavailable>,
    input_fd: RawFd,
    output_fd: RawFd,
) -> Result<(), Unavailable> {
    let endpoint_fd = endpoint.as_raw_fd();
    let fds = [input_fd, output_fd, endpoint_fd];
    let mut saved_flags = [0 as libc::c_int; 3];
    for (index, fd) in fds.iter().enumerate() {
        saved_flags[index] = get_status_flags(*fd);
        set_nonblocking(*fd, true);
    }

    let result = relay_loop(endpoint_fd, deadline, check, input_fd, output_fd);

    // Python restores the blocking mode of every descriptor in `finally` and
    // ignores failures from descriptors that are already gone.
    for (index, fd) in fds.iter().enumerate() {
        if saved_flags[index] != 0 {
            set_status_flags(*fd, saved_flags[index]);
        }
    }
    result
}

fn relay_loop(
    endpoint_fd: RawFd,
    deadline: Instant,
    check: &mut dyn FnMut() -> Result<(), Unavailable>,
    input_fd: RawFd,
    output_fd: RawFd,
) -> Result<(), Unavailable> {
    let mut to_server: Vec<u8> = Vec::new();
    let mut to_client: Vec<u8> = Vec::new();
    let mut next_check = Instant::now() + check_interval();

    loop {
        check_interrupted()?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if now >= next_check {
            check()?;
            next_check = Instant::now() + check_interval();
            continue;
        }

        let input_reads = to_server.len() < RELAY_LIMIT;
        let output_writes = !to_client.is_empty();
        let mut endpoint_events = 0;
        if to_client.len() < RELAY_LIMIT {
            endpoint_events |= libc::POLLIN;
        }
        if !to_server.is_empty() {
            endpoint_events |= libc::POLLOUT;
        }

        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(3);
        if input_reads {
            pollfds.push(libc::pollfd {
                fd: input_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if output_writes {
            pollfds.push(libc::pollfd {
                fd: output_fd,
                events: libc::POLLOUT,
                revents: 0,
            });
        }
        if endpoint_events != 0 {
            pollfds.push(libc::pollfd {
                fd: endpoint_fd,
                events: endpoint_events,
                revents: 0,
            });
        }

        let wake = deadline.min(next_check);
        let timeout = wake.saturating_duration_since(Instant::now());
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `pollfds` is initialized for every element the count covers.
        let ready =
            unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Unavailable::new("stream_failed"));
        }
        if ready == 0 {
            continue;
        }

        for pollfd in &pollfds {
            let fd = pollfd.fd;
            let revents = pollfd.revents;
            if revents == 0 {
                continue;
            }
            let readable =
                revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            let writable = revents & libc::POLLOUT != 0;

            if readable && fd == input_fd && to_server.len() < RELAY_LIMIT {
                let want = (RELAY_LIMIT - to_server.len()).min(CHUNK);
                match read_some(fd, want) {
                    Read::Bytes(bytes) => to_server.extend_from_slice(&bytes),
                    Read::WouldBlock => {}
                    Read::Closed => return Ok(()),
                    Read::Failed => return Err(Unavailable::new("stream_failed")),
                }
            }
            if readable && fd == endpoint_fd && to_client.len() < RELAY_LIMIT {
                let want = (RELAY_LIMIT - to_client.len()).min(CHUNK);
                match read_some(fd, want) {
                    Read::Bytes(bytes) => to_client.extend_from_slice(&bytes),
                    Read::WouldBlock => {}
                    Read::Closed => return Ok(()),
                    Read::Failed => return Err(Unavailable::new("stream_failed")),
                }
            }
            if writable && fd == output_fd && !to_client.is_empty() {
                match write_some(fd, &to_client) {
                    Write::Count(count) => {
                        to_client.drain(..count);
                    }
                    Write::WouldBlock => {}
                    Write::Closed => return Ok(()),
                    Write::Failed => return Err(Unavailable::new("stream_failed")),
                }
            }
            if writable && fd == endpoint_fd && !to_server.is_empty() {
                match write_some(fd, &to_server) {
                    Write::Count(count) => {
                        to_server.drain(..count);
                    }
                    Write::WouldBlock => {}
                    Write::Closed => return Ok(()),
                    Write::Failed => return Err(Unavailable::new("stream_failed")),
                }
            }
        }
    }
}

fn read_some(fd: RawFd, want: usize) -> Read {
    let mut buffer = vec![0u8; want];
    // SAFETY: `buffer` has `want` bytes; the return value is checked.
    let count = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
    if count < 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Read::WouldBlock,
            io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => Read::Closed,
            _ => Read::Failed,
        };
    }
    if count == 0 {
        return Read::Closed;
    }
    buffer.truncate(count as usize);
    Read::Bytes(buffer)
}

fn write_some(fd: RawFd, buffer: &[u8]) -> Write {
    // SAFETY: `buffer` is valid for `buffer.len()` bytes; the return value is
    // checked.
    let count = unsafe { libc::write(fd, buffer.as_ptr() as *const libc::c_void, buffer.len()) };
    if count < 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Write::WouldBlock,
            io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => Write::Closed,
            _ => Write::Failed,
        };
    }
    Write::Count(count as usize)
}

fn get_status_flags(fd: RawFd) -> libc::c_int {
    // SAFETY: `fcntl` with `F_GETFL` takes no pointer and cannot fault.
    unsafe { libc::fcntl(fd, libc::F_GETFL) }
}

fn set_nonblocking(fd: RawFd, nonblocking: bool) {
    let flags = get_status_flags(fd);
    if flags < 0 {
        return;
    }
    let updated = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    set_status_flags(fd, updated);
}

fn set_status_flags(fd: RawFd, flags: libc::c_int) {
    // SAFETY: `fcntl` with `F_SETFL` takes the integer flags directly.
    unsafe {
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}
