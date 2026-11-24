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
//! must not delay a device session by one millisecond, so publishing never waits. What it does instead
//! depends on the kind of message:
//!
//! - **State** goes through a bounded queue and is **dropped** when that fills. A dashboard missing a
//!   five-second sample is a non-event; a device waiting on an acknowledgement is not.
//! - **Retained messages** — discovery and availability — are never dropped. They are held by topic, so
//!   publishing a topic again replaces what is waiting rather than queueing behind it, and what is held
//!   is bounded by the number of topics however long the broker stays away. Dropping one would leave its
//!   topic saying whatever the broker last kept: for the bridge's own availability, that is the will's
//!   `offline`, which takes every entity down with it.
//!
//! # Reconnecting republishes everything
//!
//! There is no attempt to track which retained messages a broker still holds. On every connection the
//! client emits [`Event::Connected`], and whoever is publishing treats that as "say everything again" —
//! discovery, availability, current state. That is what makes a broker restart, a network blip and a
//! first start indistinguishable, which is one behaviour to get right instead of three.
//!
//! State queued while the broker was away is thrown out before that: it describes the past, and the
//! republish says the present.
//!
//! # A broker that stops answering is a broker that is gone
//!
//! A broker host that vanishes without closing the connection leaves the kernel retransmitting for a
//! quarter of an hour by Linux's defaults before it reports anything, and one that stays up but stops
//! reading is never reported at all — the kernel goes on acknowledging. So the client does not wait to
//! be told: a ping still unanswered when the next is due, or a write the broker will not take, ends the
//! connection and starts a new one.

use core::fmt;
use core::time::Duration;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustls::pki_types::ServerName;
use snafu::Snafu;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use url::{Host, Url};

use crate::mqtt::{ClientTls, Connect, PROTOCOL_LEVEL, Packet, PacketStream, Publish, QoS, Subscribe, Will};

/// Keepalive offered to the broker.
pub const KEEPALIVE_SECS: u16 = 60;

/// How often to send PINGREQ: comfortably inside the keepalive.
///
/// Also how long one may go unanswered: a broker that has not answered by the time the next is due is
/// taken to be gone.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// How long to wait for each step of connecting — TCP, TLS, CONNACK — before abandoning an attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one write may take before the broker is taken to have stopped reading.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shortest delay between reconnection attempts.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest delay between reconnection attempts.
pub const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// How many state messages may queue before new ones are dropped.
///
/// Retained messages do not count against it: they are never dropped, and are bounded by their topics
/// instead.
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

/// What waits to go out, in the order it was published.
///
/// Two kinds of slot share one order, so a retained message and the state that follows it still go out
/// in that order. A retained slot names only its topic: the message itself is in `retained`, which is
/// what lets a second publication of the topic replace the first in place instead of queueing again.
#[derive(Debug, Default)]
struct Queue {
    /// Publication order.
    slots: VecDeque<Slot>,
    /// The retained message waiting for each topic that has a [`Slot::Retained`].
    retained: HashMap<String, Publication>,
    /// How many [`Slot::Transient`]s there are: the number the depth limits.
    transient: usize,
    /// Live [`Publications`] handles. None left and nothing queued means the client should stop.
    senders: usize,
}

/// One place in the [`Queue`].
#[derive(Debug)]
enum Slot {
    /// State, carried whole.
    Transient(Publication),
    /// A retained message, by topic.
    Retained(String),
}

impl Queue {
    /// Queue a message, or hand it back if it is state and the queue is full.
    fn push(&mut self, publication: Publication, depth: usize) -> Result<(), Publication> {
        if publication.retain {
            if let Some(waiting) = self.retained.get_mut(&publication.topic) {
                *waiting = publication;
            } else {
                self.slots.push_back(Slot::Retained(publication.topic.clone()));
                self.retained.insert(publication.topic.clone(), publication);
            }
            return Ok(());
        }
        if self.transient >= depth {
            return Err(publication);
        }
        self.transient = self.transient.saturating_add(1);
        self.slots.push_back(Slot::Transient(publication));
        Ok(())
    }

    /// The oldest message waiting.
    fn pop(&mut self) -> Option<Publication> {
        loop {
            match self.slots.pop_front()? {
                Slot::Transient(publication) => {
                    self.transient = self.transient.saturating_sub(1);
                    return Some(publication);
                }
                // Every retained slot has its message; the lookup cannot miss, and a miss would only mean
                // there is nothing to send for that slot.
                Slot::Retained(topic) => {
                    if let Some(publication) = self.retained.remove(&topic) {
                        return Some(publication);
                    }
                }
            }
        }
    }

    /// Throw away the state, keeping the retained messages. Returns how many were thrown away.
    fn discard_transient(&mut self) -> usize {
        self.slots.retain(|slot| matches!(slot, Slot::Retained(_)));
        core::mem::take(&mut self.transient)
    }
}

/// The [`Queue`] and what the two ends need around it.
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled whenever there is something new for the receiving end: a message, or the last sender gone.
    ready: Notify,
    /// How many state messages may wait.
    depth: usize,
}

impl Shared {
    /// The queue, whatever a panicking holder left behind: every critical section leaves it consistent
    /// before anything that could panic, so a poisoned lock holds nothing worse than an unpoisoned one.
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Somewhere to publish, cloneable so every task that has something to say holds one.
///
/// Separate from [`Broker`] because publishing and reading events have different owners: one task pumps
/// the event stream while one task per device publishes.
///
/// Cloning shares the queue and not the record of dropping. The queue is what keeps a stalled broker away
/// from a device session — nothing here ever waits — and each holder logging its own run of drops says
/// whose messages were lost, where a global count would only say that some were.
#[derive(Debug)]
pub struct Publications {
    shared: Arc<Shared>,
    /// State messages dropped since this handle last got one into the queue; zero while all is well.
    dropping: u64,
}

impl Publications {
    /// A queue with nothing behind it, and the receiving end.
    ///
    /// What [`Broker::connect`] builds internally, exposed because publishing is worth exercising without a
    /// broker: whoever holds the receiver sees exactly what would have gone out, in order.
    pub fn channel(depth: usize) -> (Self, Outbox) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                senders: 1,
                ..Queue::default()
            }),
            ready: Notify::new(),
            depth,
        });
        (
            Self {
                shared: Arc::clone(&shared),
                dropping: 0,
            },
            Outbox { shared },
        )
    }

    /// Queue a message, without waiting.
    ///
    /// Returns whether it was queued. Only state can be refused: a full queue means the broker is not
    /// keeping up, so the message is dropped rather than allowed to apply backpressure to a device session.
    /// A run of drops is logged once as it starts and once as it ends, not once per message — an outage
    /// otherwise buries the log in a line per sample.
    pub fn try_publish(&mut self, publication: Publication) -> bool {
        let transient = !publication.retain;
        let pushed = self.shared.queue().push(publication, self.shared.depth);
        match pushed {
            Ok(()) => {
                self.shared.ready.notify_one();
                // Only state ends a run: a retained message is accepted however full the queue is, so it
                // says nothing about whether the broker has caught up.
                if transient && self.dropping > 0 {
                    tracing::info!(dropped = self.dropping, "publishing state again after dropping some");
                    self.dropping = 0;
                }
                true
            }
            Err(rejected) => {
                if self.dropping == 0 {
                    tracing::warn!(
                        topic = %rejected.topic,
                        "the broker is not keeping up; dropping state until it does"
                    );
                }
                self.dropping = self.dropping.saturating_add(1);
                false
            }
        }
    }
}

impl Clone for Publications {
    fn clone(&self) -> Self {
        let mut queue = self.shared.queue();
        queue.senders = queue.senders.saturating_add(1);
        drop(queue);
        Self {
            shared: Arc::clone(&self.shared),
            dropping: 0,
        }
    }
}

impl Drop for Publications {
    fn drop(&mut self) {
        let mut queue = self.shared.queue();
        queue.senders = queue.senders.saturating_sub(1);
        let last = queue.senders == 0;
        drop(queue);
        if last {
            self.shared.ready.notify_one();
        }
    }
}

/// The receiving end of [`Publications`]: everything published, in order.
#[derive(Debug)]
pub struct Outbox {
    shared: Arc<Shared>,
}

impl Outbox {
    /// The next message, waiting for one; `None` once every [`Publications`] handle is gone and the queue
    /// is empty.
    ///
    /// Cancel-safe: a message leaves the queue only in the same step that returns it, so this can sit in
    /// a `select!` arm that loses.
    pub async fn recv(&self) -> Option<Publication> {
        loop {
            {
                let mut queue = self.shared.queue();
                if let Some(publication) = queue.pop() {
                    return Some(publication);
                }
                if queue.senders == 0 {
                    return None;
                }
            }
            // A notification sent between the check above and this wait is kept as a permit, so it is
            // not lost; one left over from a message already taken only costs another look.
            self.shared.ready.notified().await;
        }
    }

    /// The next message, if one is waiting.
    pub fn try_recv(&self) -> Option<Publication> {
        self.shared.queue().pop()
    }

    /// Throw away the queued state, keeping retained messages. Returns how many were thrown away.
    fn discard_transient(&self) -> usize {
        self.shared.queue().discard_transient()
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
        Self::connect_timed(config, Timing::STANDARD)
    }

    /// [`Self::connect`], on clocks of the caller's choosing.
    fn connect_timed(config: BrokerConfig, timing: Timing) -> Result<Self, BrokerError> {
        let task = BrokerTask::new(config, timing)?;

        let (publications, outbox) = Publications::channel(QUEUE_DEPTH);
        let (events_tx, events_rx) = mpsc::channel(QUEUE_DEPTH);

        tokio::spawn(task.run(outbox, events_tx));

        Ok(Self {
            publications,
            events: events_rx,
        })
    }

    /// A handle to no broker at all, for tests of what would be published over one.
    ///
    /// The queue is handed back rather than drained by a client, so a test reads the messages themselves.
    /// No event ever arrives, which is why a test drives [`Event`]s in by hand instead of running the
    /// publisher's own loop.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, Outbox) {
        let (publications, outbox) = Publications::channel(QUEUE_DEPTH);
        let (_events_tx, events_rx) = mpsc::channel(QUEUE_DEPTH);
        (
            Self {
                publications,
                events: events_rx,
            },
            outbox,
        )
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

/// The client's clocks, gathered so a test can run them faster than a real broker needs.
#[derive(Debug, Clone, Copy)]
struct Timing {
    ping_interval: Duration,
    connect_timeout: Duration,
    write_timeout: Duration,
}

impl Timing {
    const STANDARD: Self = Self {
        ping_interval: PING_INTERVAL,
        connect_timeout: CONNECT_TIMEOUT,
        write_timeout: WRITE_TIMEOUT,
    };
}

/// The client's own state, so the connection logic is methods rather than a chain of arguments.
struct BrokerTask {
    config: BrokerConfig,
    server_name: Option<ServerName<'static>>,
    timing: Timing,
}

/// Why one broker connection ended.
enum Outcome {
    /// The handle was dropped; stop entirely.
    LocalShutdown,
    /// The connection failed or closed; worth retrying.
    Disconnected(String),
}

impl BrokerTask {
    fn new(config: BrokerConfig, timing: Timing) -> Result<Self, BrokerError> {
        // Resolved once: a name that cannot be a server name will not become one on the third attempt.
        let server_name = if config.url.tls {
            Some(config.url.server_name()?)
        } else {
            None
        };
        Ok(Self {
            config,
            server_name,
            timing,
        })
    }

    /// Connect, pump, reconnect, until the handle goes away.
    async fn run(self, publications: Outbox, events: mpsc::Sender<Event>) {
        let mut backoff = BACKOFF_MIN;

        loop {
            match self.connected(&publications, &events).await {
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
    async fn connected(&self, publications: &Outbox, events: &mpsc::Sender<Event>) -> Outcome {
        let address = self.config.url.address();
        let limit = self.timing.connect_timeout;
        tracing::info!(broker = %self.config.url, "connecting to the broker");

        // Bounded, because the kernel's own limit on an unanswered SYN is two minutes.
        let tcp = match tokio::time::timeout(limit, TcpStream::connect(&address)).await {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(error)) => return Outcome::Disconnected(format!("tcp connect: {error}")),
            Err(_) => return Outcome::Disconnected(format!("tcp connect: no answer within {limit:?}")),
        };
        drop(tcp.set_nodelay(true));

        match self.server_name.clone() {
            Some(name) => match tokio::time::timeout(limit, self.config.tls.connector().connect(name, tcp)).await {
                Ok(Ok(tls)) => {
                    self.session(PacketStream::new(tls, MAX_PACKET_LEN), publications, events)
                        .await
                }
                Ok(Err(error)) => Outcome::Disconnected(format!("tls: {error}")),
                Err(_) => Outcome::Disconnected(format!("tls: no handshake within {limit:?}")),
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
        publications: &Outbox,
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

        // Before the announcement, so nothing republished for this connection can be among what is thrown
        // out. The state left over from the outage would only replay the past ahead of the present.
        let discarded = publications.discard_transient();
        if discarded > 0 {
            tracing::info!(discarded, "discarded state queued while the broker was away");
        }

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

        self.send(stream, &connect, "connect").await?;

        let limit = self.timing.connect_timeout;
        match tokio::time::timeout(limit, stream.next_packet()).await {
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
            Err(_) => Err(format!("no CONNACK within {limit:?}")),
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
        self.send(stream, &subscribe, "subscribe").await
    }

    /// Publish what is queued, deliver what arrives, keep the connection alive.
    async fn pump<S>(
        &self,
        stream: &mut PacketStream<S>,
        publications: &Outbox,
        events: &mpsc::Sender<Event>,
    ) -> Outcome
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ping = tokio::time::interval(self.timing.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick completes immediately

        let mut packet_id: u16 = 2;
        // Whether a ping has gone out with nothing heard since. Any packet clears it, not only PINGRESP:
        // whatever the broker sends shows it is there and reading.
        let mut unanswered = false;

        loop {
            tokio::select! {
                outgoing = publications.recv() => {
                    let Some(publication) = outgoing else {
                        // The handle was dropped. Say goodbye, which also tells the broker not to publish
                        // the will: this is an orderly shutdown, not a death.
                        drop(self.send(stream, &Packet::Disconnect, "disconnect").await);
                        return Outcome::LocalShutdown;
                    };

                    let packet = Self::publish_packet(publication, &mut packet_id);
                    if let Err(reason) = self.send(stream, &packet, "write").await {
                        return Outcome::Disconnected(reason);
                    }
                }

                incoming = stream.next_packet() => {
                    if matches!(incoming, Ok(Some(_))) {
                        unanswered = false;
                    }
                    match incoming {
                        Ok(Some(Packet::Publish(publish))) => {
                            if let Some(outcome) = self.deliver(stream, publish, events).await {
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
                    if unanswered {
                        return Outcome::Disconnected(format!(
                            "no answer to a ping within {:?}",
                            self.timing.ping_interval
                        ));
                    }
                    if let Err(reason) = self.send(stream, &Packet::PingReq, "ping").await {
                        return Outcome::Disconnected(reason);
                    }
                    unanswered = true;
                }
            }
        }
    }

    /// Write one packet, giving up if the broker does not take it in time.
    ///
    /// A write that blocks means the send buffer is full: the broker has stopped reading, or its host has
    /// gone and the kernel is still retransmitting. The connection is no use either way, and waiting on it
    /// would stop everything else the pump does — including noticing that.
    async fn send<S>(&self, stream: &mut PacketStream<S>, packet: &Packet, what: &str) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limit = self.timing.write_timeout;
        match tokio::time::timeout(limit, stream.send(packet)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("{what}: {error}")),
            Err(_) => Err(format!("{what}: the broker took nothing for {limit:?}")),
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
    async fn deliver<S>(
        &self,
        stream: &mut PacketStream<S>,
        publish: Publish,
        events: &mpsc::Sender<Event>,
    ) -> Option<Outcome>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if let (QoS::AtLeastOnce, Some(packet_id)) = (publish.qos, publish.packet_id)
            && let Err(reason) = self.send(stream, &Packet::PubAck { packet_id }, "puback").await
        {
            return Some(Outcome::Disconnected(reason));
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
    use core::time::Duration;

    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::{Broker, BrokerConfig, BrokerUrl, DEFAULT_PORT, DEFAULT_TLS_PORT, Event, Publication, Timing};
    use crate::mqtt::{ClientTls, Packet, PacketStream, QoS};

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

    /// What a [`broker`] saw: a connection accepted, or a message published to it.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Accepted,
        Published { topic: String, payload: Vec<u8> },
    }

    /// A broker on `listener` that accepts every connection and answers its CONNECT.
    ///
    /// One that is `silent` then goes quiet with the connection held open — the broker that has stopped
    /// reading, which is the case the kernel never reports.
    async fn broker(listener: TcpListener, silent: bool, seen: mpsc::UnboundedSender<Seen>) {
        while let Ok((tcp, _)) = listener.accept().await {
            drop(seen.send(Seen::Accepted));
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut stream = PacketStream::new(tcp, 1 << 20);
                drop(stream.next_packet().await);
                drop(
                    stream
                        .send(&Packet::ConnAck {
                            session_present: false,
                            code: 0,
                        })
                        .await,
                );
                if silent {
                    core::future::pending::<()>().await;
                }
                while let Ok(Some(packet)) = stream.next_packet().await {
                    match packet {
                        Packet::Publish(publish) => drop(seen.send(Seen::Published {
                            topic: publish.topic,
                            payload: publish.payload,
                        })),
                        Packet::PingReq => drop(stream.send(&Packet::PingResp).await),
                        _ => {}
                    }
                }
            });
        }
    }

    fn config(port: u16) -> BrokerConfig {
        BrokerConfig {
            url: BrokerUrl::parse(&format!("mqtt://127.0.0.1:{port}")).unwrap(),
            client_id: "test".to_owned(),
            username: None,
            password: None,
            subscriptions: Vec::new(),
            will: None,
            tls: ClientTls::from_env().unwrap(),
        }
    }

    /// Clocks fast enough that a broker going quiet is noticed within a test.
    const FAST: Timing = Timing {
        ping_interval: Duration::from_millis(100),
        connect_timeout: Duration::from_secs(1),
        write_timeout: Duration::from_secs(1),
    };

    /// What the broker sees until it has been quiet for `quiet`.
    async fn collect(seen: &mut mpsc::UnboundedReceiver<Seen>, quiet: Duration) -> Vec<Seen> {
        let mut out = Vec::new();
        while let Ok(Some(next)) = tokio::time::timeout(quiet, seen.recv()).await {
            out.push(next);
        }
        out
    }

    #[tokio::test]
    async fn retained_messages_outlast_an_outage_and_stale_state_does_not() {
        // The broker is away: nothing listens on the port.
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut client = Broker::connect_timed(config(port), FAST).unwrap();

        // The outage, as a device sees it: telemetry far beyond the queue's depth, and an availability
        // that changes on the way. Only the last word on a retained topic matters.
        client.try_publish(Publication::retained("bridge/availability", b"offline".to_vec()));
        for sample in 0..1000 {
            client.try_publish(Publication::state("device/state", format!("{sample}").into_bytes()));
        }
        assert!(
            client.try_publish(Publication::retained("bridge/availability", b"online".to_vec())),
            "a retained message must be taken however full the queue is"
        );

        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let (tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(broker(listener, false, tx));
        assert_eq!(client.next_event().await, Some(Event::Connected));
        client.try_publish(Publication::state("device/state", b"fresh".to_vec()));

        let published: Vec<(String, Vec<u8>)> = collect(&mut seen, Duration::from_millis(500))
            .await
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Published { topic, payload } => Some((topic, payload)),
                Seen::Accepted => None,
            })
            .collect();
        assert_eq!(
            published,
            vec![
                ("bridge/availability".to_owned(), b"online".to_vec()),
                ("device/state".to_owned(), b"fresh".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn a_broker_that_stops_answering_is_left_for_a_new_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(broker(listener, true, tx));

        let mut client = Broker::connect_timed(config(port), FAST).unwrap();
        assert_eq!(client.next_event().await, Some(Event::Connected));
        assert_eq!(seen.recv().await, Some(Seen::Accepted));

        // Two ping intervals to notice, the first backoff, and room to spare.
        let again = tokio::time::timeout(Duration::from_secs(5), seen.recv()).await;
        assert_eq!(again.ok().flatten(), Some(Seen::Accepted), "never reconnected");
    }

    #[tokio::test]
    async fn a_broker_that_answers_is_kept() {
        // The other side of the ping check: an answering broker must not be dropped for a silence it does
        // not have.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(broker(listener, false, tx));

        let mut client = Broker::connect_timed(config(port), FAST).unwrap();
        assert_eq!(client.next_event().await, Some(Event::Connected));
        let accepted = collect(&mut seen, Duration::from_secs(1))
            .await
            .into_iter()
            .filter(|seen| *seen == Seen::Accepted)
            .count();
        assert_eq!(accepted, 1, "reconnected to a broker that was answering every ping");
    }
}
