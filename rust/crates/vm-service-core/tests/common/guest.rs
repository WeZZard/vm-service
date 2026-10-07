//! An in-process guest for guest sessions (`docs/arch/guest-session.md`).
//!
//! Each channel the service opens runs the real `guest-session-agent` request
//! loop on a thread, over OS pipes, in a temporary home. A channel can be cut
//! after the agent's hello, which stands in for a link that fails while a
//! request is in flight: both directions stop, and the agent sees its input
//! end, as it does when `sshd` closes the session. A channel can also go
//! silent after the hello without ending, as a link does when the network
//! drops packets: nothing arrives and nothing ends until the host gives up
//! and ends the channel.

use std::collections::VecDeque;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use guest_session::agent::{hex, serve, AgentOptions};
use sha2::{Digest, Sha256};

use vm_service_core::error::OpResult;
use vm_service_core::guest_link::{ChannelProcess, GuestChannel};

/// The agent bytes the fake host offers by default.
pub const AGENT: &[u8] = b"fake guest-session-agent build 1";

/// A fault planned for one channel.
#[derive(Clone, Copy, Debug)]
pub enum Cut {
    /// Cut the link once the agent has sent this many bytes after its hello.
    ///
    /// The client reads frames on its own thread, so `0` cuts the link as
    /// soon as the hello is read, before any request is sent.
    AgentOutputAfterHello(usize),
    /// Cut the link once the host has sent this many bytes.
    HostInputAfter(usize),
    /// After the hello, deliver nothing from the agent, without ending, until
    /// the channel is ended; the host's input still reaches the agent.
    SilenceAfterHello,
    /// After the hello, deliver nothing either way: the agent's output is
    /// held and the host's writes block, until the channel is ended.
    StallAfterHello,
}

/// The guest side of the fake host.
pub struct FakeGuest {
    /// The guest user's home directory.
    pub home: PathBuf,
    /// The agent bytes the host offers for upload.
    pub offered: Mutex<Vec<u8>>,
    /// The agent bytes installed in the guest, if any.
    pub installed: Mutex<Option<Vec<u8>>>,
    /// Channels opened, including those that found no agent.
    pub opens: AtomicUsize,
    /// Agent uploads received.
    pub uploads: AtomicUsize,
    /// Agents still running.
    pub live: Arc<AtomicUsize>,
    /// Faults for the next channels that reach an agent, in order.
    pub cuts: Mutex<VecDeque<Option<Cut>>>,
    /// The alive-frame interval of agents started from now on.
    pub heartbeat: Mutex<Option<Duration>>,
    links: Mutex<Vec<Arc<Link>>>,
}

impl FakeGuest {
    /// A guest with no agent installed.
    pub fn new(home: PathBuf) -> Self {
        std::fs::create_dir_all(&home).expect("guest home");
        Self {
            home,
            offered: Mutex::new(AGENT.to_vec()),
            installed: Mutex::new(None),
            opens: AtomicUsize::new(0),
            uploads: AtomicUsize::new(0),
            live: Arc::new(AtomicUsize::new(0)),
            cuts: Mutex::new(VecDeque::new()),
            heartbeat: Mutex::new(None),
            links: Mutex::new(Vec::new()),
        }
    }

    /// Plan a fault for the next channel that reaches an agent.
    pub fn plan(&self, cut: Option<Cut>) {
        self.cuts.lock().expect("cuts").push_back(cut);
    }

    /// Cut every open link now.
    pub fn cut_all(&self) {
        for link in self.links.lock().expect("links").iter() {
            link.cut();
        }
    }

    /// Wait until no agent runs, or panic after `limit`.
    pub fn wait_no_agents(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.live.load(Ordering::SeqCst) > 0 {
            assert!(
                Instant::now() < deadline,
                "{} guest session agents still run",
                self.live.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Store an uploaded agent, as the upload command does.
    pub fn install(&self, bytes: Vec<u8>) {
        self.uploads.fetch_add(1, Ordering::SeqCst);
        *self.installed.lock().expect("installed") = Some(bytes);
    }

    /// Open a channel to the installed agent, or to a shell that finds none.
    pub fn open(&self) -> OpResult<GuestChannel> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let installed = self.installed.lock().expect("installed").clone();
        let Some(installed) = installed else {
            return Ok(GuestChannel {
                reader: Box::new(io::empty()),
                writer: Box::new(io::sink()),
                process: Box::new(Missing),
            });
        };
        let plan = self.cuts.lock().expect("cuts").pop_front().flatten();
        let (host_reader, agent_writer) = io::pipe().expect("pipe");
        let (agent_reader, host_writer) = io::pipe().expect("pipe");
        let link = Arc::new(Link {
            cut: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            to_agent: Mutex::new(Some(host_writer)),
        });
        self.links.lock().expect("links").push(Arc::clone(&link));
        let done = Arc::new(AtomicBool::new(false));
        let options = AgentOptions {
            shell: PathBuf::from("/bin/sh"),
            home: self.home.clone(),
            build: hex(&Sha256::digest(&installed)),
            heartbeat: *self.heartbeat.lock().expect("heartbeat"),
        };
        self.live.fetch_add(1, Ordering::SeqCst);
        let live = Arc::clone(&self.live);
        let finished = Arc::clone(&done);
        std::thread::spawn(move || {
            let _ = serve(agent_reader, agent_writer, options);
            live.fetch_sub(1, Ordering::SeqCst);
            finished.store(true, Ordering::SeqCst);
        });
        let (output_limit, input_limit) = match plan {
            Some(Cut::AgentOutputAfterHello(count)) => (Some(count), None),
            Some(Cut::HostInputAfter(count)) => (None, Some(count)),
            Some(Cut::SilenceAfterHello | Cut::StallAfterHello) | None => (None, None),
        };
        Ok(GuestChannel {
            reader: Box::new(CutReader {
                inner: host_reader,
                link: Arc::clone(&link),
                seen: 0,
                header: Vec::new(),
                limit: output_limit,
                freeze: plan
                    .filter(|cut| matches!(cut, Cut::SilenceAfterHello | Cut::StallAfterHello)),
            }),
            writer: Box::new(CutWriter {
                link: Arc::clone(&link),
                remaining: input_limit,
            }),
            process: Box::new(FakeChannel { link, done }),
        })
    }
}

/// Both directions of one channel.
struct Link {
    cut: AtomicBool,
    /// The agent's output is held.
    silent: AtomicBool,
    /// The host's writes block.
    stalled: AtomicBool,
    to_agent: Mutex<Option<PipeWriter>>,
}

impl Link {
    /// Stop both directions; the agent sees its input end.
    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        self.to_agent.lock().expect("to_agent").take();
    }
}

/// Agent output, ending when the link is cut.
struct CutReader {
    inner: PipeReader,
    link: Arc<Link>,
    seen: usize,
    header: Vec<u8>,
    limit: Option<usize>,
    /// Silence or stall the link once the hello has been read.
    freeze: Option<Cut>,
}

impl CutReader {
    /// The hello frame's length, once its header has been read.
    fn hello(&self) -> Option<usize> {
        let header = self.header.get(..16)?;
        let length = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
        Some(16 + 4 + length + 4)
    }

    /// Bytes this reader may still return, if limited.
    fn allowance(&self) -> Option<usize> {
        let limit = if self.freeze.is_some() {
            0
        } else {
            self.limit?
        };
        match self.hello() {
            Some(hello) => Some((hello + limit).saturating_sub(self.seen)),
            // The hello's length is not known yet; let its header through.
            None => Some(16 - self.header.len()),
        }
    }
}

impl Read for CutReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.link.cut.load(Ordering::SeqCst) {
                return Ok(0);
            }
            if self.allowance() == Some(0) && self.freeze.is_none() {
                self.link.cut();
                return Ok(0);
            }
            if self.link.silent.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let mut poll = libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&mut poll, 1, 20) };
            if ready > 0 {
                break;
            }
        }
        let wanted = match self.allowance() {
            Some(allowed) => buffer.len().min(allowed),
            None => buffer.len(),
        };
        let count = self.inner.read(&mut buffer[..wanted])?;
        let missing = 16usize.saturating_sub(self.header.len()).min(count);
        self.header.extend_from_slice(&buffer[..missing]);
        self.seen += count;
        if self.allowance() == Some(0) {
            // Freeze before the host sees the hello, so no request gets through.
            if let Some(cut) = self.freeze {
                self.link.silent.store(true, Ordering::SeqCst);
                if matches!(cut, Cut::StallAfterHello) {
                    self.link.stalled.store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(count)
    }
}

/// Host input, failing once the link is cut.
struct CutWriter {
    link: Arc<Link>,
    remaining: Option<usize>,
}

impl Write for CutWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.remaining == Some(0) {
            self.link.cut();
        }
        while self.link.stalled.load(Ordering::SeqCst) && !self.link.cut.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut target = self.link.to_agent.lock().expect("to_agent");
        let Some(writer) = target.as_mut() else {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        };
        let wanted = match self.remaining {
            Some(remaining) => buffer.len().min(remaining),
            None => buffer.len(),
        };
        let count = writer.write(&buffer[..wanted])?;
        if let Some(remaining) = self.remaining.as_mut() {
            *remaining -= count;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for CutWriter {
    fn drop(&mut self) {
        // Closing the channel's input, as dropping a child's stdin does.
        self.link.to_agent.lock().expect("to_agent").take();
    }
}

/// The channel process for a running agent.
struct FakeChannel {
    link: Arc<Link>,
    done: Arc<AtomicBool>,
}

impl ChannelProcess for FakeChannel {
    fn finish(&mut self, grace: Duration) {
        self.link.to_agent.lock().expect("to_agent").take();
        let deadline = Instant::now() + grace;
        while !self.done.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn abort_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
        let link = Arc::clone(&self.link);
        Some(Box::new(move || link.cut()))
    }
}

/// The channel process of a start command that found no agent.
struct Missing;

impl ChannelProcess for Missing {
    fn finish(&mut self, _grace: Duration) {}

    fn diagnostics(&self) -> String {
        "no agent in the guest (exit 127)".to_string()
    }
}
