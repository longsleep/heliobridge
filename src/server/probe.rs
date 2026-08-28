//! A liveness probe on the device-facing port, answered over HTTP.
//!
//! # Why here rather than on a socket of its own
//!
//! The port is already bound and its address is already configuration every process reads, so a probe on
//! it needs no second listener, no path, no name to discover, and nothing written to disk. It also
//! exercises the real service path: an answer proves the accept loop is running and the runtime is
//! scheduling, which is what a container healthcheck should mean.
//!
//! # Why HTTP
//!
//! Because it exists, and anything can speak it. `curl http://127.0.0.1:7006/healthz` works from any host
//! that has curl, an orchestrator can probe it directly, and `/healthz` is already the path the control
//! API answers — one spelling of the question rather than two. MQTT's own PINGREQ would be two octets
//! rather than a request line, but only something speaking MQTT could send one, and answering it outside a
//! session is not what the specification provides for.
//!
//! Nothing here is a general HTTP server: a request line is read, bounded, answered and closed. No
//! routing, no header parsing, no body, no keep-alive.
//!
//! # How a probe is told apart from a device
//!
//! Every TLS connection starts with a handshake record, whose first octet is `0x16`; an HTTP request
//! starts with a method. So the first octets decide, with no ambiguity:
//!
//! - `0x16` — a device. Not ours; the octets are left untouched for the handshake.
//! - `GET ` — ours.
//! - anything else — not ours either.
//!
//! The connection is **peeked, never read**, until it is known to be HTTP. Claiming a connection and then
//! discovering it was a device would have consumed the octets the handshake needed. That requirement is
//! also why [`Probe`] is written against a concrete [`TcpStream`] rather than being generic over
//! `AsyncRead`: peeking is not part of any async I/O trait, and a probe that could not peek would have to
//! guess.
//!
//! # What it deliberately does not report
//!
//! Anything about the device. A datalogger sleeps for hours at a time, so a health signal that depended on
//! one being connected would report a container as broken every night — and restarting it would neither
//! wake the device nor preserve the recording. Device presence belongs in `/devices` and in Home
//! Assistant, where a human reads it.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use snafu::{ResultExt, Snafu};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The path that answers `200`, spelled as the control API spells it.
pub const HEALTH_PATH: &str = "/healthz";

/// The only method worth answering.
const METHOD: &[u8] = b"GET ";

/// First octet of a TLS handshake record, and so of every connection a device makes.
const TLS_HANDSHAKE: u8 = 0x16;

/// Longest request line entertained, after which the connection is not worth reading.
const MAX_REQUEST_LINE: usize = 256;

/// How long the whole exchange may take. Generous for a loopback request already in flight.
const TIMEOUT: Duration = Duration::from_millis(500);

/// Whether a connection was a probe, and therefore already finished with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It spoke HTTP. It has been answered and the caller should drop the connection.
    Answered,
    /// It did not. **No octet has been consumed**, so the caller proceeds as usual.
    NotAProbe,
}

/// What a probe is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Response {
    /// Serving, and scheduling well enough to say so.
    Healthy,
    /// A path this does not answer, so a mistyped probe fails rather than reporting health.
    Unknown,
}

impl Response {
    /// The response for a request line's path.
    fn to(path: &str) -> Self {
        if path == HEALTH_PATH {
            Self::Healthy
        } else {
            Self::Unknown
        }
    }

    /// The whole reply, headers and body.
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Healthy => {
                b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n"
            }
            Self::Unknown => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        }
    }
}

impl AsRef<[u8]> for Response {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

/// A request line, once one has been read in full.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestLine(String);

impl RequestLine {
    /// The request target, or empty when the line is malformed — which [`Response::to`] answers as
    /// unknown, so a malformed line cannot report health.
    fn path(&self) -> &str {
        self.0.split(' ').nth(1).unwrap_or_default()
    }
}

/// One accepted connection, being asked whether it is a probe.
#[derive(Debug)]
pub struct Probe<'stream> {
    stream: &'stream mut TcpStream,
}

impl<'stream> Probe<'stream> {
    /// Consider a freshly accepted connection.
    pub const fn new(stream: &'stream mut TcpStream) -> Self {
        Self { stream }
    }

    /// Answer it if it is speaking HTTP, and say whether it was.
    pub async fn serve(mut self) -> Outcome {
        match tokio::time::timeout(TIMEOUT, self.exchange()).await {
            Ok(outcome) => outcome,
            // A timeout is not a probe: ours is written immediately after connecting.
            Err(_) => Outcome::NotAProbe,
        }
    }

    async fn exchange(&mut self) -> Outcome {
        if !self.is_http().await {
            return Outcome::NotAProbe;
        }

        let Some(line) = self.read_request_line().await else {
            // It began as HTTP and then stopped. The octets are spent either way, so the connection is
            // ours to close; there is nothing useful to answer.
            return Outcome::Answered;
        };

        let response = Response::to(line.path());
        if let Err(error) = self.stream.write_all(response.as_ref()).await {
            tracing::debug!(%error, "could not answer a probe");
        }
        // Best effort: the peer has what it needs, and a failed shutdown changes nothing.
        drop(self.stream.shutdown().await);
        Outcome::Answered
    }

    /// Whether the head of the connection is an HTTP `GET`, without consuming anything.
    async fn is_http(&self) -> bool {
        let mut buffer = [0_u8; METHOD.len()];
        loop {
            // Closed before saying anything, or the peek failed: not a probe either way.
            let seen = match self.stream.peek(&mut buffer).await {
                Ok(0) | Err(_) => return false,
                Ok(seen) => seen,
            };
            let Some(head) = buffer.get(..seen) else { return false };

            // One octet rules out a device, and any disagreeing prefix rules out HTTP — so a stranger is
            // handed on without waiting for the timeout.
            if head.first() == Some(&TLS_HANDSHAKE) || !METHOD.starts_with(head) {
                return false;
            }
            if seen == METHOD.len() {
                return true;
            }
            // A split write: wait for the rest rather than deciding on a prefix.
            tokio::task::yield_now().await;
        }
    }

    /// Consume up to the end of the request line.
    async fn read_request_line(&mut self) -> Option<RequestLine> {
        let mut line = Vec::with_capacity(MAX_REQUEST_LINE);
        let mut byte = [0_u8; 1];
        while line.len() < MAX_REQUEST_LINE {
            if self.stream.read_exact(&mut byte).await.is_err() {
                return None;
            }
            if byte[0] == b'\n' {
                while line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line).ok().map(RequestLine);
            }
            line.push(byte[0]);
        }
        None
    }
}

/// Why a check could not report health.
#[derive(Debug, Snafu)]
pub enum CheckError {
    /// Nothing accepted the connection.
    #[snafu(display("could not reach {address}: {source}"))]
    Connect {
        /// Where the check tried.
        address: SocketAddr,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// Connected, but the exchange failed.
    #[snafu(display("no answer from {address}: {source}"))]
    Exchange {
        /// Where the check tried.
        address: SocketAddr,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// It answered, but not with health.
    #[snafu(display("{address} answered {status}"))]
    Unhealthy {
        /// Where the check tried.
        address: SocketAddr,
        /// The status line it sent back.
        status: String,
    },
    /// It answered something that is not HTTP.
    #[snafu(display("{address} did not answer HTTP; is something else listening?"))]
    NotHttp {
        /// Where the check tried.
        address: SocketAddr,
    },
}

/// Asks a running server whether it is alive.
///
/// The counterpart of [`Probe`], kept beside it so the request and the reply cannot drift apart.
#[derive(Debug, Clone, Copy)]
pub struct Check {
    address: SocketAddr,
}

impl Check {
    /// Check a specific address.
    pub const fn new(address: SocketAddr) -> Self {
        Self { address }
    }

    /// Check whatever a server with this bind address would be serving.
    ///
    /// A wildcard bind is not an address to connect to, so it becomes loopback on the same port — which
    /// is also the only address the server answers a probe from.
    pub fn for_listener(listen: SocketAddr) -> Self {
        let address = if listen.ip().is_unspecified() {
            match listen {
                SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, listen.port())),
                SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::LOCALHOST, listen.port())),
            }
        } else {
            listen
        };
        Self::new(address)
    }

    /// Ask, and say nothing on success.
    ///
    /// # Errors
    ///
    /// [`CheckError`] if the server could not be reached, did not answer, or answered anything but 200.
    pub async fn run(self) -> Result<(), CheckError> {
        let address = self.address;
        let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))
            .and_then(|result| result)
            .context(ConnectSnafu { address })?;

        let status = tokio::time::timeout(TIMEOUT, Self::exchange(&mut stream))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))
            .and_then(|result| result)
            .context(ExchangeSnafu { address })?;

        let mut fields = status.split(' ');
        let version = fields.next().unwrap_or_default();
        if !version.starts_with("HTTP/") {
            return NotHttpSnafu { address }.fail();
        }
        if fields.next() == Some("200") {
            Ok(())
        } else {
            UnhealthySnafu { address, status }.fail()
        }
    }

    /// Send the request and return the status line.
    async fn exchange(stream: &mut TcpStream) -> std::io::Result<String> {
        let request = format!("GET {HEALTH_PATH} HTTP/1.0\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;

        let mut reply = String::new();
        stream.read_to_string(&mut reply).await?;
        Ok(reply.lines().next().unwrap_or_default().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{Check, HEALTH_PATH, Outcome, Probe, RequestLine, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A listener on loopback, and a client connected to it.
    async fn pair() -> (TcpListener, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        let client = TcpStream::connect(address).await.expect("connect");
        (listener, client)
    }

    async fn answer_to(request: &str) -> String {
        let (listener, mut client) = pair().await;
        client.write_all(request.as_bytes()).await.expect("write the request");

        let (mut accepted, _) = listener.accept().await.expect("accept");
        assert_eq!(Probe::new(&mut accepted).serve().await, Outcome::Answered);

        let mut reply = String::new();
        client.read_to_string(&mut reply).await.expect("read the reply");
        reply
    }

    #[test]
    fn a_malformed_request_line_cannot_report_health() {
        assert_eq!(
            Response::to(RequestLine("nonsense".to_owned()).path()),
            Response::Unknown
        );
    }

    #[tokio::test]
    async fn the_health_path_answers_ok() {
        let reply = answer_to(&format!("GET {HEALTH_PATH} HTTP/1.0\r\n\r\n")).await;
        assert!(reply.starts_with("HTTP/1.0 200 OK"), "{reply}");
        assert!(reply.ends_with("ok\n"), "{reply}");
    }

    #[tokio::test]
    async fn another_path_is_not_reported_as_healthy() {
        let reply = answer_to("GET /elsewhere HTTP/1.0\r\n\r\n").await;
        assert!(reply.starts_with("HTTP/1.0 404"), "{reply}");
    }

    #[tokio::test]
    async fn a_tls_client_is_left_untouched() {
        let (listener, mut client) = pair().await;
        // A ClientHello record header: the octets a device sends first.
        let hello = [0x16_u8, 0x03, 0x01, 0x00, 0x2c];
        client.write_all(&hello).await.expect("write a hello");

        let (mut accepted, _) = listener.accept().await.expect("accept");
        assert_eq!(Probe::new(&mut accepted).serve().await, Outcome::NotAProbe);

        // Nothing was consumed, so the handshake still sees its first octets.
        let mut seen = [0_u8; 5];
        accepted.read_exact(&mut seen).await.expect("read the hello");
        assert_eq!(seen, hello);
    }

    #[tokio::test]
    async fn another_method_keeps_its_octets() {
        let (listener, mut client) = pair().await;
        client.write_all(b"POST / HTTP/1.1\r\n").await.expect("write");

        let (mut accepted, _) = listener.accept().await.expect("accept");
        assert_eq!(Probe::new(&mut accepted).serve().await, Outcome::NotAProbe);

        let mut seen = [0_u8; 4];
        accepted.read_exact(&mut seen).await.expect("read");
        assert_eq!(&seen, b"POST");
    }

    #[tokio::test]
    async fn a_check_and_a_probe_agree() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let (mut accepted, _) = listener.accept().await.expect("accept");
            Probe::new(&mut accepted).serve().await
        });

        Check::new(address).run().await.expect("a served probe reports health");
    }

    #[tokio::test]
    async fn a_check_against_nothing_fails() {
        // Bind and drop, so the port is one nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        drop(listener);

        assert!(Check::new(address).run().await.is_err());
    }

    #[test]
    fn a_wildcard_bind_is_checked_on_loopback() {
        let check = Check::for_listener("0.0.0.0:7006".parse().expect("an address"));
        assert_eq!(check.address, "127.0.0.1:7006".parse().expect("an address"));

        // A specific address is checked where it was bound.
        let check = Check::for_listener("192.0.2.10:7006".parse().expect("an address"));
        assert_eq!(check.address, "192.0.2.10:7006".parse().expect("an address"));
    }

    #[tokio::test]
    async fn a_silent_connection_is_not_a_probe() {
        let (listener, client) = pair().await;
        let (mut accepted, _) = listener.accept().await.expect("accept");
        // Closing without writing must not be mistaken for a probe.
        drop(client);
        assert_eq!(Probe::new(&mut accepted).serve().await, Outcome::NotAProbe);
    }
}
