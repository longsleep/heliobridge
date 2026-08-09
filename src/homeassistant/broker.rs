//! An MQTT client for a broker this program does not own.
//!
//! The device-facing side of the program is an MQTT *server*; this is the other direction, publishing to
//! whatever broker Home Assistant already listens to. It is deliberately built on the same codec as
//! everything else here rather than on a client library: the codec is already a client — that is how the
//! cloud relay reaches Growatt — and a library would add a second MQTT implementation, a second TLS stack
//! and a second set of dependencies to do what this one does.
//!
//! # It must never hold the device up
//!
//! The broker is downstream of everything. A broker that is unreachable, slow, or refusing connections
//! must not delay a device session by one millisecond, so publications go through a bounded queue and are
//! **dropped and counted** when it fills. A dashboard missing a five-second sample is a non-event; a
//! device waiting on an acknowledgement is not.
//!
//! # Reconnecting republishes everything
//!
//! There is no attempt to track which retained messages a broker still holds. On every connection the
//! client emits [`Event::Connected`], and whoever is publishing treats that as "say everything again" —
//! discovery, availability, current state. That is what makes a broker restart, a network blip and a
//! first start indistinguishable, which is one behaviour to get right instead of three.

use core::fmt;
use core::time::Duration;
use std::net::IpAddr;

use rustls::pki_types::ServerName;
use snafu::Snafu;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use url::{Host, Url};

use crate::mqtt::{ClientTls, Connect, PROTOCOL_LEVEL, Packet, PacketStream, Publish, QoS, Subscribe, Will};

/// Keepalive offered to the broker.
pub const KEEPALIVE_SECS: u16 = 60;

/// How often to send PINGREQ: comfortably inside the keepalive.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// How long to wait for CONNACK before abandoning an attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Shortest delay between reconnection attempts.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest delay between reconnection attempts.
pub const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// How many publications may queue before new ones are dropped.
///
/// Deep enough for a full discovery burst — every entity for a device, published back to back on
/// reconnect — since dropping part of that would leave Home Assistant with half a device.
pub const QUEUE_DEPTH: usize = 256;

/// Largest packet accepted from the broker.
pub const MAX_PACKET_LEN: usize = 256 * 1024;

/// Default port for `mqtt://`.
pub const DEFAULT_PORT: u16 = 1883;

/// Default port for `mqtts://`.
pub const DEFAULT_TLS_PORT: u16 = 8883;

/// Why the broker client could not be set up.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum BrokerError {
    /// The URL was not one this understands.
    #[snafu(display("{url} is not an MQTT broker URL: expected mqtt://host[:port] or mqtts://host[:port]"))]
    Malformed {
        /// What was given.
        url: String,
    },

    /// The port was not a number.
    #[snafu(display("{port} is not a port number"))]
    Port {
        /// What was given.
        port: String,
    },

    /// The host cannot be a TLS server name.
    #[snafu(display("{host} cannot be verified as a TLS server name; use a host name rather than an address"))]
    UnverifiableHost {
        /// What was given.
        host: String,
    },
}

/// A broker's host: a name to resolve, or an address already known.
///
/// Kept apart because everything downstream treats them differently. A name is dialled and verified as a
/// name; an address is dialled in whatever syntax the transport wants — IPv6 in brackets — and verified
/// against an IP entry in the certificate. Carrying one string for both is what leads to stripping
/// brackets by hand in three places and forgetting one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerHost {
    /// A DNS name.
    Name(String),
    /// A literal address.
    Address(IpAddr),
}

impl fmt::Display for BrokerHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            // The bracketed form for IPv6, which is what a URL and a socket address both want.
            Self::Address(IpAddr::V6(address)) => write!(f, "[{address}]"),
            Self::Address(address) => write!(f, "{address}"),
        }
    }
}

/// Where the broker is, and whether to speak TLS to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerUrl {
    /// Host name or address.
    pub host: BrokerHost,
    /// Port.
    pub port: u16,
    /// Whether the connection is wrapped in TLS.
    pub tls: bool,
}

impl BrokerUrl {
    /// Parse `mqtt://host[:port]` or `mqtts://host[:port]`.
    ///
    /// Parsing is the [`url`] crate's, which is what makes the host **typed** — a name, an IPv4 address or
    /// an IPv6 address — rather than a string that every consumer has to take apart again. The traps are
    /// all in that step: an IPv6 literal is full of colons, so the port is not "whatever follows the last
    /// one", and the brackets belong in a socket address but not in a TLS server name.
    ///
    /// What it rejects is anything a broker URL has no use for — a path, a query, embedded credentials.
    /// Credentials have their own settings, and accepting a form that is then ignored is worse than
    /// refusing it.
    ///
    /// # Errors
    ///
    /// [`BrokerError::Malformed`] if it is not a URL of that shape, [`BrokerError::Port`] if a port was
    /// given that is not a number in range.
    pub fn parse(url: &str) -> Result<Self, BrokerError> {
        let invalid = || BrokerError::Malformed { url: url.to_owned() };

        let parsed = Url::parse(url).map_err(|_| invalid())?;
        let tls = match parsed.scheme() {
            "mqtt" | "tcp" => false,
            "mqtts" | "ssl" | "mqtt+ssl" => true,
            _ => return Err(invalid()),
        };

        if !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(invalid());
        }

        // A port that is present but unusable makes the whole URL malformed rather than defaulting: a
        // misspelled port would otherwise connect somewhere the operator never asked for and look fine.
        // `Url` refuses to parse those, so reaching here with no port means none was written.
        let host = match parsed.host().ok_or_else(invalid)? {
            Host::Domain(name) => BrokerHost::Name(name.to_owned()),
            Host::Ipv4(address) => BrokerHost::Address(IpAddr::V4(address)),
            Host::Ipv6(address) => BrokerHost::Address(IpAddr::V6(address)),
        };

        Ok(Self {
            host,
            port: parsed
                .port()
                .unwrap_or(if tls { DEFAULT_TLS_PORT } else { DEFAULT_PORT }),
            tls,
        })
    }

    /// `host:port`, for connecting and for logs.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The host as a TLS server name.
    ///
    /// # Errors
    ///
    /// [`BrokerError::UnverifiableHost`] if a name is not one TLS can verify.
    pub fn server_name(&self) -> Result<ServerName<'static>, BrokerError> {
        match &self.host {
            // An address needs no parsing: it is verified against an IP entry in the certificate, and the
            // brackets an IPv6 URL carries are a URL detail that never reaches here.
            BrokerHost::Address(address) => Ok(ServerName::IpAddress((*address).into())),
            BrokerHost::Name(name) => {
                ServerName::try_from(name.clone()).map_err(|_| BrokerError::UnverifiableHost { host: name.clone() })
            }
        }
    }
}

impl fmt::Display for BrokerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "mqtts" } else { "mqtt" };
        write!(f, "{scheme}://{}:{}", self.host, self.port)
    }
}

/// Everything needed to reach a broker.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Where it is.
    pub url: BrokerUrl,
    /// Client identifier to present.
    pub client_id: String,
    /// Username, if the broker wants one.
    pub username: Option<String>,
    /// Password, if the broker wants one.
    pub password: Option<String>,
    /// Topic filters to subscribe to on every connection.
    pub subscriptions: Vec<String>,
    /// What the broker publishes if this connection dies without saying goodbye.
    pub will: Option<Will>,
    /// The process's outbound TLS configuration. Unused for a plain connection.
    pub tls: ClientTls,
}

/// One message to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// Topic to publish on.
    pub topic: String,
    /// The payload.
    pub payload: Vec<u8>,
    /// Delivery guarantee.
    pub qos: QoS,
    /// Whether the broker keeps it as the topic's last known value.
    pub retain: bool,
}

impl Publication {
    /// A retained message: discovery and availability, which a subscriber must see even if it connects
    /// after the fact.
    pub fn retained(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: QoS::AtLeastOnce,
            retain: true,
        }
    }

    /// A transient message: state, which is replaced by the next one a few seconds later and is worthless
    /// to a subscriber that arrives late.
    pub fn state(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: QoS::AtMostOnce,
            retain: false,
        }
    }
}

/// Somewhere to publish, cloneable so every task that has something to say holds one.
///
/// Separate from [`Broker`] because publishing and reading events have different owners: one task pumps
/// the event stream while one task per device publishes.
///
/// Cloning shares the queue and not the drop count. The queue is what keeps a stalled broker away from a
/// device session — nothing here ever waits — and each holder counting its own drops is the number worth
/// reporting, since a global total would say a message was lost without saying whose.
#[derive(Debug, Clone)]
pub struct Publications {
    sender: mpsc::Sender<Publication>,
    dropped: u64,
}

impl Publications {
    /// A queue with nothing behind it, and the receiving end.
    ///
    /// What [`Broker::connect`] builds internally, exposed because publishing is worth exercising without a
    /// broker: whoever holds the receiver sees exactly what would have gone out, in order.
    pub fn channel(depth: usize) -> (Self, mpsc::Receiver<Publication>) {
        let (sender, receiver) = mpsc::channel(depth);
        (Self { sender, dropped: 0 }, receiver)
    }

    /// Queue a message, without waiting.
    ///
    /// Returns whether it was queued. A full queue means the broker is not keeping up, so the message is
    /// dropped and counted rather than allowed to apply backpressure to a device session.
    pub fn try_publish(&mut self, publication: Publication) -> bool {
        // The value comes back out of the error, so the ordinary path clones nothing.
        let Err(rejected) = self.sender.try_send(publication) else {
            return true;
        };
        self.dropped = self.dropped.saturating_add(1);
        tracing::warn!(
            topic = %rejected.into_inner().topic,
            dropped = self.dropped,
            "dropped a message: the broker is not keeping up"
        );
        false
    }

    /// How many publications this handle dropped for a broker that was not keeping up.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Something that happened on the broker connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A connection was established. Everything retained must be published again.
    Connected,
    /// A message arrived on a subscribed topic.
    Message {
        /// Topic it arrived on.
        topic: String,
        /// The payload.
        payload: Vec<u8>,
    },
}

/// A handle to the broker connection.
///
/// Dropping it stops the client, after a DISCONNECT so the broker does not publish the will for an
/// orderly shutdown.
#[derive(Debug)]
pub struct Broker {
    publications: Publications,
    events: mpsc::Receiver<Event>,
}

impl Broker {
    /// Connect, and keep connected.
    ///
    /// Returns immediately: the connection is made in the background and retried for as long as the handle
    /// lives, so a broker that is down at startup is not a startup failure.
    ///
    /// # Errors
    ///
    /// [`BrokerError::UnverifiableHost`] if TLS was asked for and the host cannot be a server name — checked
    /// once here rather than on every attempt.
    pub fn connect(config: BrokerConfig) -> Result<Self, BrokerError> {
        let task = BrokerTask::new(config)?;

        let (publications, publications_rx) = Publications::channel(QUEUE_DEPTH);
        let (events_tx, events_rx) = mpsc::channel(QUEUE_DEPTH);

        tokio::spawn(task.run(publications_rx, events_tx));

        Ok(Self {
            publications,
            events: events_rx,
        })
    }

    /// A handle for publishing, for a task that has something to say but no interest in events.
    pub fn publications(&self) -> Publications {
        self.publications.clone()
    }

    /// Queue a message, without waiting. See [`Publications::try_publish`].
    pub fn try_publish(&mut self, publication: Publication) -> bool {
        self.publications.try_publish(publication)
    }

    /// The next thing that happened, or `None` once the client has stopped.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

/// The client's own state, so the connection logic is methods rather than a chain of arguments.
struct BrokerTask {
    config: BrokerConfig,
    server_name: Option<ServerName<'static>>,
}

/// Why one broker connection ended.
enum Outcome {
    /// The handle was dropped; stop entirely.
    LocalShutdown,
    /// The connection failed or closed; worth retrying.
    Disconnected(String),
}

impl BrokerTask {
    fn new(config: BrokerConfig) -> Result<Self, BrokerError> {
        // Resolved once: a name that cannot be a server name will not become one on the third attempt.
        let server_name = if config.url.tls {
            Some(config.url.server_name()?)
        } else {
            None
        };
        Ok(Self { config, server_name })
    }

    /// Connect, pump, reconnect, until the handle goes away.
    async fn run(self, mut publications: mpsc::Receiver<Publication>, events: mpsc::Sender<Event>) {
        let mut backoff = BACKOFF_MIN;

        loop {
            match self.connected(&mut publications, &events).await {
                Outcome::LocalShutdown => {
                    tracing::debug!("broker client stopping");
                    return;
                }
                Outcome::Disconnected(reason) => {
                    tracing::warn!(
                        broker = %self.config.url,
                        reason = %reason,
                        retry_in_s = backoff.as_secs(),
                        "broker connection lost; the device is unaffected"
                    );
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
        }
    }

    /// One connection, from CONNECT to failure.
    ///
    /// The transport is decided here and the rest is generic over it, which is the whole of the
    /// plain-versus-TLS difference: MQTT does not care what it is written to.
    async fn connected(&self, publications: &mut mpsc::Receiver<Publication>, events: &mpsc::Sender<Event>) -> Outcome {
        let address = self.config.url.address();
        tracing::info!(broker = %self.config.url, "connecting to the broker");

        let tcp = match TcpStream::connect(&address).await {
            Ok(tcp) => tcp,
            Err(error) => return Outcome::Disconnected(format!("tcp connect: {error}")),
        };
        drop(tcp.set_nodelay(true));

        match self.server_name.clone() {
            Some(name) => match self.config.tls.connector().connect(name, tcp).await {
                Ok(tls) => {
                    self.session(PacketStream::new(tls, MAX_PACKET_LEN), publications, events)
                        .await
                }
                Err(error) => Outcome::Disconnected(format!("tls: {error}")),
            },
            None => {
                self.session(PacketStream::new(tcp, MAX_PACKET_LEN), publications, events)
                    .await
            }
        }
    }

    /// Handshake, subscribe, then pump until something ends it.
    async fn session<S>(
        &self,
        mut stream: PacketStream<S>,
        publications: &mut mpsc::Receiver<Publication>,
        events: &mpsc::Sender<Event>,
    ) -> Outcome
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if let Err(reason) = self.handshake(&mut stream).await {
            return Outcome::Disconnected(reason);
        }
        if let Err(reason) = self.subscribe(&mut stream).await {
            return Outcome::Disconnected(reason);
        }

        tracing::info!(broker = %self.config.url, "broker connected");
        // Announced before anything is pumped, so a publisher republishes retained state before the first
        // transient message rather than after it.
        if events.send(Event::Connected).await.is_err() {
            return Outcome::LocalShutdown;
        }

        self.pump(&mut stream, publications, events).await
    }

    /// CONNECT and CONNACK.
    async fn handshake<S>(&self, stream: &mut PacketStream<S>) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let connect = Packet::Connect(Connect {
            protocol_level: PROTOCOL_LEVEL,
            client_id: self.config.client_id.clone(),
            username: self.config.username.clone(),
            password: self.config.password.as_ref().map(|p| p.as_bytes().to_vec()),
            keepalive: KEEPALIVE_SECS,
            // No session state is wanted: everything is republished on connect anyway, and a broker
            // holding queued messages for this client across a restart would deliver a backlog of
            // commands that have since been superseded.
            clean_session: true,
            will: self.config.will.clone(),
        });

        stream
            .send(&connect)
            .await
            .map_err(|error| format!("connect: {error}"))?;

        match tokio::time::timeout(CONNECT_TIMEOUT, stream.next_packet()).await {
            Ok(Ok(Some(Packet::ConnAck { code: 0, .. }))) => Ok(()),
            // Code 4 and 5 are bad credentials and not authorised, which are configuration problems rather
            // than transient ones — worth naming, since the retry loop would otherwise repeat them forever
            // with nothing to distinguish them from a broker that is merely down.
            Ok(Ok(Some(Packet::ConnAck { code, .. }))) => Err(match code {
                4 => "broker rejected the credentials (CONNACK 4)".to_owned(),
                5 => "broker refused authorisation (CONNACK 5)".to_owned(),
                other => format!("broker refused the connection, code {other}"),
            }),
            Ok(Ok(other)) => Err(format!(
                "expected CONNACK, got {}",
                other.map_or("nothing", |packet| packet.kind())
            )),
            Ok(Err(error)) => Err(format!("read: {error}")),
            Err(_) => Err("no CONNACK before the timeout".to_owned()),
        }
    }

    /// Subscribe to every configured filter, in one packet.
    async fn subscribe<S>(&self, stream: &mut PacketStream<S>) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if self.config.subscriptions.is_empty() {
            return Ok(());
        }
        let subscribe = Packet::Subscribe(Subscribe {
            packet_id: 1,
            filters: self
                .config
                .subscriptions
                .iter()
                .map(|filter| (filter.clone(), QoS::AtLeastOnce.bits()))
                .collect(),
        });
        stream
            .send(&subscribe)
            .await
            .map_err(|error| format!("subscribe: {error}"))
    }

    /// Publish what is queued, deliver what arrives, keep the connection alive.
    async fn pump<S>(
        &self,
        stream: &mut PacketStream<S>,
        publications: &mut mpsc::Receiver<Publication>,
        events: &mpsc::Sender<Event>,
    ) -> Outcome
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick completes immediately

        let mut packet_id: u16 = 2;

        loop {
            tokio::select! {
                outgoing = publications.recv() => {
                    let Some(publication) = outgoing else {
                        // The handle was dropped. Say goodbye, which also tells the broker not to publish
                        // the will: this is an orderly shutdown, not a death.
                        drop(stream.send(&Packet::Disconnect).await);
                        return Outcome::LocalShutdown;
                    };

                    let packet = Self::publish_packet(publication, &mut packet_id);
                    if let Err(error) = stream.send(&packet).await {
                        return Outcome::Disconnected(format!("write: {error}"));
                    }
                }

                incoming = stream.next_packet() => {
                    match incoming {
                        Ok(Some(Packet::Publish(publish))) => {
                            if let Some(outcome) = Self::deliver(stream, publish, events).await {
                                return outcome;
                            }
                        }
                        Ok(Some(Packet::PingResp | Packet::PubAck { .. } | Packet::SubAck { .. })) => {}
                        Ok(Some(Packet::Disconnect)) => {
                            return Outcome::Disconnected("broker sent DISCONNECT".to_owned());
                        }
                        Ok(Some(other)) => {
                            tracing::debug!(kind = other.kind(), "ignoring an unexpected packet from the broker");
                        }
                        Ok(None) => return Outcome::Disconnected("broker closed the connection".to_owned()),
                        Err(error) => return Outcome::Disconnected(format!("read: {error}")),
                    }
                }

                _ = ping.tick() => {
                    if let Err(error) = stream.send(&Packet::PingReq).await {
                        return Outcome::Disconnected(format!("ping: {error}"));
                    }
                }
            }
        }
    }

    /// Wrap a publication, allocating a packet identifier where the QoS needs one.
    fn publish_packet(publication: Publication, packet_id: &mut u16) -> Packet {
        let id = if publication.qos == QoS::AtMostOnce {
            None
        } else {
            // Wrapping past zero, which is not a valid identifier.
            *packet_id = packet_id.checked_add(1).unwrap_or(1);
            Some(*packet_id)
        };
        Packet::Publish(Publish {
            topic: publication.topic,
            qos: publication.qos,
            retain: publication.retain,
            dup: false,
            packet_id: id,
            payload: publication.payload,
        })
    }

    /// Hand an incoming message on, acknowledging it first.
    async fn deliver<S>(stream: &mut PacketStream<S>, publish: Publish, events: &mpsc::Sender<Event>) -> Option<Outcome>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if let (QoS::AtLeastOnce, Some(packet_id)) = (publish.qos, publish.packet_id)
            && let Err(error) = stream.send(&Packet::PubAck { packet_id }).await
        {
            return Some(Outcome::Disconnected(format!("puback: {error}")));
        }

        let event = Event::Message {
            topic: publish.topic,
            payload: publish.payload,
        };
        events.send(event).await.err().map(|_| Outcome::LocalShutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::{BrokerUrl, DEFAULT_PORT, DEFAULT_TLS_PORT, Publication};
    use crate::mqtt::QoS;

    #[test]
    fn both_schemes_parse_with_their_own_default_port() {
        let plain = BrokerUrl::parse("mqtt://192.168.1.10").expect("plain");
        assert_eq!(plain.host.to_string(), "192.168.1.10");
        assert_eq!(plain.port, DEFAULT_PORT);
        assert!(!plain.tls);

        let secure = BrokerUrl::parse("mqtts://broker.example").expect("tls");
        assert_eq!(secure.host.to_string(), "broker.example");
        assert_eq!(secure.port, DEFAULT_TLS_PORT);
        assert!(secure.tls);
    }

    #[test]
    fn an_explicit_port_wins_over_the_default() {
        let url = BrokerUrl::parse("mqtt://192.168.1.10:1884").expect("parse");
        assert_eq!(url.port, 1884);
        assert_eq!(url.address(), "192.168.1.10:1884");
        assert_eq!(url.to_string(), "mqtt://192.168.1.10:1884");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_and_finds_its_port() {
        // The case that makes hand-written parsing wrong: the host is full of colons, so the port is not
        // "whatever follows the last one", and the brackets have to survive into the dialled address.
        let with_port = BrokerUrl::parse("mqtt://[2001:db8::1]:1884").expect("parse");
        assert_eq!(with_port.host.to_string(), "[2001:db8::1]");
        assert_eq!(with_port.port, 1884);
        assert_eq!(with_port.address(), "[2001:db8::1]:1884");

        let without_port = BrokerUrl::parse("mqtt://[::1]").expect("parse");
        assert_eq!(without_port.host.to_string(), "[::1]");
        assert_eq!(without_port.port, DEFAULT_PORT);
    }

    #[test]
    fn a_trailing_slash_is_tolerated_but_a_path_is_not() {
        assert!(BrokerUrl::parse("mqtt://host/").is_ok());
        assert!(BrokerUrl::parse("mqtt://host/topic").is_err());
        assert!(BrokerUrl::parse("mqtt://host?retain=1").is_err());
    }

    #[test]
    fn what_is_refused_is_refused_with_a_reason() {
        // Credentials belong in their own settings, so a URL carrying them would be silently half-honoured.
        for url in ["", "host:1883", "http://host", "mqtt://", "mqtt://user:pass@host"] {
            assert!(BrokerUrl::parse(url).is_err(), "{url} should not parse");
        }
        // A port that was written but is not usable must not quietly become the default: that would
        // connect somewhere the operator did not ask for and look like it worked.
        for url in ["mqtt://host:not-a-port", "mqtt://host:99999"] {
            assert!(BrokerUrl::parse(url).is_err(), "{url} should not parse");
        }

        // An empty port is not the same thing. The URL standard says it means "no port given", so it takes
        // the default rather than being an error.
        assert_eq!(BrokerUrl::parse("mqtt://host:").expect("parse").port, DEFAULT_PORT);
    }

    #[test]
    fn a_tls_broker_may_be_named_or_addressed() {
        // Both are accepted: rustls verifies a name against a DNS entry in the certificate and an address
        // against an IP entry. Addressing a broker by IP is therefore allowed here, and fails later
        // against a certificate carrying no IP — which is the honest place for it to fail, since whether
        // it works depends on the certificate rather than on the URL.
        for url in ["mqtts://broker.example", "mqtts://192.168.1.10", "mqtts://[::1]"] {
            let parsed = BrokerUrl::parse(url).expect("parse");
            assert!(parsed.server_name().is_ok(), "{url}");
        }
    }

    #[test]
    fn retained_and_transient_publications_differ_in_both_respects() {
        // Discovery must survive a subscriber arriving late; state must not, since it is replaced every
        // few seconds and a retained copy would be read as current long after it stopped being true.
        let discovery = Publication::retained("homeassistant/sensor/x/config", b"{}".to_vec());
        assert!(discovery.retain);
        assert_eq!(discovery.qos, QoS::AtLeastOnce);

        let state = Publication::state("heliobridge/x/state", b"{}".to_vec());
        assert!(!state.retain);
        assert_eq!(state.qos, QoS::AtMostOnce);
    }
}
