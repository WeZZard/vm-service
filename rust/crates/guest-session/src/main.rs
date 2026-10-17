//! `guest-session-agent serve`: the guest end of a guest session, on standard
//! input and output. See `docs/arch/guest-session.md`.

use std::fs::File;
use std::io::BufReader;
use std::os::fd::FromRawFd;

use guest_session::agent::{serve, AgentOptions};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve"] => {
            // SAFETY: descriptors 0 and 1 are open for the life of the process
            // and nothing else in this program uses them. Raw files avoid the
            // line buffering of `Stdout`, which would split binary frames.
            let (input, output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
            let code = match serve(
                BufReader::with_capacity(1 << 20, input),
                output,
                AgentOptions::from_environment(),
            ) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("guest-session-agent: {error}");
                    1
                }
            };
            std::process::exit(code);
        }
        ["--version"] => println!("guest-session-agent {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("usage: guest-session-agent serve");
            std::process::exit(2);
        }
    }
}
