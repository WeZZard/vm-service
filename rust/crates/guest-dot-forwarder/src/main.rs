//! `vm-service-dot-forwarder --listen 127.0.0.1:53 --upstream 1.1.1.1#cloudflare-dns.com ...`
//!
//! Answers plain DNS on the listen address, over UDP and TCP, through DNS
//! over TLS to the upstreams, tried in order (DC-16).

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::process::ExitCode;
use std::sync::Arc;

use guest_dot_forwarder::{serve_tcp, serve_udp, tls_config, Resolver, Upstream};

const USAGE: &str =
    "usage: vm-service-dot-forwarder [--listen ADDRESS:PORT] --upstream A.B.C.D[#NAME] [--upstream ...]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("vm-service-dot-forwarder: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut listen: SocketAddr = "127.0.0.1:53".parse().expect("valid default");
    let mut upstreams = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                let value = arguments.next().ok_or(USAGE)?;
                listen = value
                    .parse()
                    .map_err(|_| format!("invalid --listen {value:?}"))?;
            }
            "--upstream" => upstreams.push(Upstream::parse(&arguments.next().ok_or(USAGE)?)?),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if upstreams.is_empty() {
        return Err(USAGE.to_string());
    }
    let resolver = Arc::new(Resolver::new(upstreams, tls_config()));
    let udp = UdpSocket::bind(listen).map_err(|error| format!("udp {listen}: {error}"))?;
    let tcp = TcpListener::bind(listen).map_err(|error| format!("tcp {listen}: {error}"))?;
    eprintln!("vm-service-dot-forwarder: listening on {listen}");
    let tcp_resolver = resolver.clone();
    std::thread::spawn(move || {
        if let Err(error) = serve_tcp(tcp, tcp_resolver) {
            eprintln!("vm-service-dot-forwarder: tcp: {error}");
        }
    });
    serve_udp(udp, resolver).map_err(|error| format!("udp: {error}"))
}
