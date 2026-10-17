//! A minimal DNS-over-TLS forwarder for guests whose system resolver cannot
//! be pointed at DNS over TLS by a script (DC-16): a macOS guest.
//!
//! It answers plain DNS on a loopback address, over UDP and TCP, by sending
//! each query over TLS (RFC 7858) to public resolvers, so a host that
//! intercepts plain DNS on port 53 never sees the guest's queries.
//!
//! The forwarder checks each resolver's certificate against Mozilla's roots,
//! built in, and against the resolver's name when one is given
//! (`1.1.1.1#cloudflare-dns.com`), otherwise against its address.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// The DNS-over-TLS port.
pub const DOT_PORT: u16 = 853;
/// How long one upstream exchange may take.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// A public resolver reached over TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// Its address.
    pub address: Ipv4Addr,
    /// The name its certificate is checked against, when given.
    pub name: Option<String>,
}

impl Upstream {
    /// Parse `a.b.c.d` or `a.b.c.d#name`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (address, name) = match text.split_once('#') {
            Some((address, name)) => (address, Some(name)),
            None => (text, None),
        };
        let address: Ipv4Addr = address
            .trim()
            .parse()
            .map_err(|_| format!("invalid upstream {text:?}: expected a.b.c.d or a.b.c.d#name"))?;
        let name = match name.map(str::trim) {
            None => None,
            Some(name) if valid_name(name) => Some(name.to_string()),
            Some(_) => {
                return Err(format!(
                    "invalid upstream {text:?}: the name is not a host name"
                ))
            }
        };
        Ok(Self { address, name })
    }

    fn server_name(&self) -> ServerName<'static> {
        match &self.name {
            Some(name) => ServerName::try_from(name.clone())
                .unwrap_or_else(|_| ServerName::IpAddress(IpAddr::V4(self.address).into())),
            None => ServerName::IpAddress(IpAddr::V4(self.address).into()),
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// A TLS client configuration that trusts Mozilla's roots.
pub fn tls_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

type Connection = StreamOwned<ClientConnection, TcpStream>;

/// Sends queries to the upstreams in order, reusing one idle connection per
/// upstream.
pub struct Resolver {
    upstreams: Vec<Upstream>,
    config: Arc<ClientConfig>,
    idle: Vec<Mutex<Option<Connection>>>,
}

impl Resolver {
    /// A resolver over `upstreams`.
    pub fn new(upstreams: Vec<Upstream>, config: Arc<ClientConfig>) -> Self {
        let idle = upstreams.iter().map(|_| Mutex::new(None)).collect();
        Self {
            upstreams,
            config,
            idle,
        }
    }

    /// Answer one DNS message, or `None` when no upstream answered.
    pub fn resolve(&self, query: &[u8]) -> Option<Vec<u8>> {
        for (index, upstream) in self.upstreams.iter().enumerate() {
            let reused = self.idle[index]
                .lock()
                .ok()
                .and_then(|mut slot| slot.take());
            if let Some(mut connection) = reused {
                if let Ok(answer) = exchange(&mut connection, query) {
                    self.keep(index, connection);
                    return Some(answer);
                }
            }
            match self.connect(upstream).and_then(|mut connection| {
                let answer = exchange(&mut connection, query)?;
                Ok((connection, answer))
            }) {
                Ok((connection, answer)) => {
                    self.keep(index, connection);
                    return Some(answer);
                }
                Err(error) => eprintln!("upstream {}: {error}", upstream.address),
            }
        }
        None
    }

    fn keep(&self, index: usize, connection: Connection) {
        if let Ok(mut slot) = self.idle[index].lock() {
            *slot = Some(connection);
        }
    }

    fn connect(&self, upstream: &Upstream) -> std::io::Result<Connection> {
        let address = SocketAddr::new(IpAddr::V4(upstream.address), DOT_PORT);
        let socket = TcpStream::connect_timeout(&address, UPSTREAM_TIMEOUT)?;
        socket.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
        socket.set_write_timeout(Some(UPSTREAM_TIMEOUT))?;
        socket.set_nodelay(true)?;
        let connection = ClientConnection::new(self.config.clone(), upstream.server_name())
            .map_err(std::io::Error::other)?;
        Ok(StreamOwned::new(connection, socket))
    }
}

/// Write one length-prefixed DNS message and read one back (RFC 1035 4.2.2,
/// as DNS over TLS frames it).
pub fn exchange<S: Read + Write>(stream: &mut S, query: &[u8]) -> std::io::Result<Vec<u8>> {
    write_framed(stream, query)?;
    stream.flush()?;
    read_framed(stream)
}

/// Write a message with its two-byte length.
pub fn write_framed<W: Write>(stream: &mut W, message: &[u8]) -> std::io::Result<()> {
    let length = u16::try_from(message.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "message too long"))?;
    let mut framed = Vec::with_capacity(message.len() + 2);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(message);
    stream.write_all(&framed)
}

/// Read a message with its two-byte length.
pub fn read_framed<R: Read>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let mut length = [0u8; 2];
    stream.read_exact(&mut length)?;
    let mut message = vec![0u8; usize::from(u16::from_be_bytes(length))];
    stream.read_exact(&mut message)?;
    Ok(message)
}

/// A SERVFAIL answer to `query`: its ID, opcode and RD bit, no sections.
/// `None` when the query is too short to carry a header.
pub fn servfail(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut answer = vec![0u8; 12];
    answer[0] = query[0];
    answer[1] = query[1];
    // QR = 1, the query's opcode and RD bit.
    answer[2] = 0x80 | (query[2] & 0x79);
    // RA = 1, RCODE = 2 (server failure).
    answer[3] = 0x82;
    Some(answer)
}

/// Serve plain DNS over UDP on `socket`, one thread per query.
pub fn serve_udp(socket: UdpSocket, resolver: Arc<Resolver>) -> std::io::Result<()> {
    let socket = Arc::new(socket);
    let mut buffer = [0u8; 4096];
    loop {
        let (length, peer) = socket.recv_from(&mut buffer)?;
        let query = buffer[..length].to_vec();
        let socket = socket.clone();
        let resolver = resolver.clone();
        std::thread::spawn(move || {
            if let Some(answer) = resolver.resolve(&query).or_else(|| servfail(&query)) {
                let _ = socket.send_to(&answer, peer);
            }
        });
    }
}

/// Serve plain DNS over TCP on `listener`, one thread per connection.
pub fn serve_tcp(listener: TcpListener, resolver: Arc<Resolver>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let resolver = resolver.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            while let Ok(query) = read_framed(&mut stream) {
                let Some(answer) = resolver.resolve(&query).or_else(|| servfail(&query)) else {
                    break;
                };
                if write_framed(&mut stream, &answer).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstreams_parse_with_or_without_a_name() {
        assert_eq!(
            Upstream::parse("1.1.1.1#cloudflare-dns.com").unwrap(),
            Upstream {
                address: "1.1.1.1".parse().unwrap(),
                name: Some("cloudflare-dns.com".into())
            }
        );
        assert_eq!(
            Upstream::parse("9.9.9.9").unwrap(),
            Upstream {
                address: "9.9.9.9".parse().unwrap(),
                name: None
            }
        );
        for bad in [
            "dns.google",
            "1.1.1.1#",
            "1.1.1.1#bad name",
            "::1",
            "1.1.1.1#-x.com",
        ] {
            assert!(Upstream::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_names_fall_back_to_the_address() {
        let named = Upstream::parse("8.8.8.8#dns.google").unwrap();
        assert_eq!(
            format!("{:?}", named.server_name()),
            format!("{:?}", ServerName::try_from("dns.google").unwrap())
        );
        let bare = Upstream::parse("8.8.8.8").unwrap();
        assert!(matches!(bare.server_name(), ServerName::IpAddress(_)));
    }

    #[test]
    fn framing_round_trips() {
        let mut wire = Vec::new();
        write_framed(&mut wire, b"\x12\x34hello").unwrap();
        assert_eq!(&wire[..2], &[0, 7]);
        let mut cursor = std::io::Cursor::new(wire);
        assert_eq!(read_framed(&mut cursor).unwrap(), b"\x12\x34hello");
    }

    /// A stream that records what is written and replays a canned answer.
    struct Canned {
        written: Vec<u8>,
        answer: std::io::Cursor<Vec<u8>>,
    }

    impl Read for Canned {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.answer.read(buffer)
        }
    }

    impl Write for Canned {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_exchange_frames_the_query_and_unframes_the_answer() {
        let mut stream = Canned {
            written: Vec::new(),
            answer: std::io::Cursor::new(vec![0, 3, 1, 2, 3]),
        };
        assert_eq!(exchange(&mut stream, &[9, 9]).unwrap(), vec![1, 2, 3]);
        assert_eq!(stream.written, vec![0, 2, 9, 9]);
    }

    #[test]
    fn servfail_keeps_the_id_and_rd_and_sets_rcode_2() {
        // ID 0xbeef, RD set, one question.
        let query = [0xbe, 0xef, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 7, b'e'];
        let answer = servfail(&query).unwrap();
        assert_eq!(answer, vec![0xbe, 0xef, 0x81, 0x82, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(servfail(&[0; 5]).is_none());
    }

    #[test]
    fn a_resolver_with_no_reachable_upstream_answers_none() {
        // 192.0.2.1 is TEST-NET-1; nothing answers there.
        let resolver = Resolver::new(vec![Upstream::parse("192.0.2.1").unwrap()], tls_config());
        let started = std::time::Instant::now();
        assert!(resolver.resolve(&[0; 12]).is_none());
        assert!(started.elapsed() <= UPSTREAM_TIMEOUT + Duration::from_secs(2));
    }

    /// Resolves through the real public resolvers; needs the internet.
    #[test]
    #[ignore = "needs the internet and TCP 853 to 1.1.1.1"]
    fn live_a_query_is_answered_over_tls() {
        let resolver = Resolver::new(
            vec![Upstream::parse("1.1.1.1#cloudflare-dns.com").unwrap()],
            tls_config(),
        );
        // A query for example.com A, ID 0x1234, RD.
        let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["example", "com"] {
            query.push(label.len() as u8);
            query.extend_from_slice(label.as_bytes());
        }
        query.extend_from_slice(&[0, 0, 1, 0, 1]);
        let answer = resolver.resolve(&query).unwrap();
        assert_eq!(&answer[..2], &[0x12, 0x34]);
        assert_eq!(answer[3] & 0x0f, 0, "rcode");
        assert!(u16::from_be_bytes([answer[6], answer[7]]) >= 1, "answers");
    }
}
