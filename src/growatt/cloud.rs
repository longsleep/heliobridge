//! Optional relay to the Growatt cloud.
//!
//! With this on, the device's traffic reaches the vendor as well as this bridge, so the phone app and any
//! cloud-backed integration keep working. It is the one part of the program that talks *to* Growatt
//! rather than replacing them, which is why it lives with the vendor's protocol rather than beside the
//! server: the endpoint, the credentials and the topics are all Growatt's.
//!
//! # It connects as the device
//!
//! Upstream, this is not a new client with its own identity — it presents the device's client identifier,
//! username, password and keepalive, and re-encodes the device's own CONNECT octet for octet. The cloud
//! is a third party that may care about any of those details, so they are reproduced rather than
//! approximated. That is also why the relay uses this crate's own MQTT codec instead of a client library:
//! a library decides those details for you, which is convenient everywhere except here.
//!
//! # Local operation must not depend on it
//!
//! The device has nowhere else to publish. So every failure mode here — cloud unreachable, TLS refused,
//! connection dropped mid-session — is contained: the relay retries with backoff while the device session
//! carries on, and messages queued for a cloud that is not answering are **dropped and counted** rather
//! than allowed to apply backpressure to the device.
//!
//! # Hop by hop, not transparent
//!
//! Acknowledgements terminate at each hop: the server acknowledges the device, and the relay separately
//! acknowledges the cloud. A transparent TCP proxy would forward the device's own PUBACK upstream. The
//! difference is deliberate — it means a stalled cloud cannot delay an acknowledgement the device is
//! waiting for.

use core::future::Future;
use core::time::Duration;

use rustls::pki_types::ServerName;
use snafu::Snafu;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;

use crate::driver::upstream::{self, Endpoint, Message, Target};
use crate::mqtt::{ClientTls, Connect, PROTOCOL_LEVEL, Packet, PacketStream, Publish, QoS, Subscribe};

/// Names a certificate presented to the device must carry, common name first.
///
/// The device dials `mqtt.growatt.com` and reaches this program instead, so what it is offered has to look
/// like what it expected. This device does not verify it (F9), which is a fact about one firmware rather
/// than a promise about the next.
pub const CERTIFICATE_NAMES: &[&str] = &["*.growatt.com", "mqtt.growatt.com"];

/// Default cloud endpoint.
pub const DEFAULT_HOST: &str = "mqtt.growatt.com";

/// Default cloud port.
pub const DEFAULT_PORT: u16 = 7006;

/// Password the device presents, and therefore the one the relay presents.
///
/// A firmware constant shared across the product line, not a secret.
pub const DEVICE_PASSWORD: &[u8] = b"Growatt";

/// Keepalive the device asks for, reused upstream.
pub const KEEPALIVE_SECS: u16 = 420;

/// How often to send PINGREQ upstream: comfortably inside the keepalive.
pub const PING_INTERVAL: Duration = Duration::from_secs(150);

/// How long to wait for the cloud's CONNACK before abandoning an attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Shortest delay between reconnection attempts.
pub const BACKOFF_MIN: Duration = Duration::from_secs(2);

/// Longest delay between reconnection attempts.
pub const BACKOFF_MAX: Duration = Duration::from_mins(2);

/// How many messages may queue in either direction before new ones are dropped.
///
/// Small on purpose. A backlog of stale telemetry is worth less than the memory it occupies, and the
/// device publishes a fresh copy every five seconds.
pub const QUEUE_DEPTH: usize = 32;

/// Largest packet accepted from the cloud.
pub const MAX_PACKET_LEN: usize = 64 * 1024;

/// Why the relay could not be set up.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum RelayError {
    /// The host name is not usable for TLS.
    #[snafu(display("{host} is not a valid TLS server name"))]
    InvalidHost {
        /// The name given.
        host: String,
    },
}

impl From<upstream::InvalidHost> for RelayError {
    fn from(error: upstream::InvalidHost) -> Self {
        Self::InvalidHost { host: error.host }
    }
}

/// The session's end of the relay.
///
/// Dropping this stops the relay: the device session's lifetime is the relay's lifetime, which is what
/// the vendor's own server sees anyway — the previous proxy opened a cloud connection per device
/// connection and the cloud accepted it.
#[derive(Debug)]
pub struct Relay {
    to_cloud: mpsc::Sender<Message>,
    from_cloud: mpsc::Receiver<Message>,
    dropped: u64,
}

impl Relay {
    /// Start a relay for one device session.
    ///
    /// The TLS configuration comes in already built rather than being made here: a relay is created per
    /// device connection, and parsing every trust anchor on each reconnect would be waste.
    ///
    /// # Errors
    ///
    /// [`RelayError::InvalidHost`] if the configured host cannot be a TLS server name. Failures to
    /// *connect* are not errors here: the task retries, and the session continues either way.
    pub fn start(device_id: &str, target: Target) -> Result<Self, RelayError> {
        let task = RelayTask::new(device_id, target)?;

        let (to_cloud_tx, to_cloud_rx) = mpsc::channel(QUEUE_DEPTH);
        let (from_cloud_tx, from_cloud_rx) = mpsc::channel(QUEUE_DEPTH);

        tokio::spawn(task.run(to_cloud_rx, from_cloud_tx));

        Ok(Self {
            to_cloud: to_cloud_tx,
            from_cloud: from_cloud_rx,
            dropped: 0,
        })
    }

    /// How many messages this session failed to hand to the cloud.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl upstream::Relay for Relay {
    /// A full queue means the cloud is not keeping up, and the message is dropped: the device is waiting
    /// on this server, not on Growatt.
    fn try_forward(&mut self, message: Message) -> bool {
        if self.to_cloud.try_send(message).is_ok() {
            return true;
        }
        self.dropped = self.dropped.saturating_add(1);
        false
    }

    fn next_from_cloud(&mut self) -> impl Future<Output = Option<Message>> + Send {
        self.from_cloud.recv()
    }
}

/// The relay's own state, so the connection logic is methods rather than a chain of six-argument
/// functions.
struct RelayTask {
    device_id: String,
    config: Endpoint,
    server_name: ServerName<'static>,
    tls: ClientTls,
}

/// Why one relay connection ended.
enum Outcome {
    /// The device session went away; stop entirely.
    LocalShutdown,
    /// The cloud connection failed or closed; worth retrying.
    Disconnected(String),
}

impl RelayTask {
    fn new(device_id: &str, target: Target) -> Result<Self, RelayError> {
        let server_name = target.endpoint.server_name()?;
        Ok(Self {
            device_id: device_id.to_owned(),
            config: target.endpoint,
            server_name,
            tls: target.tls,
        })
    }

    /// Connect, pump, reconnect. Returns when the session drops its [`Relay`].
    async fn run(self, mut to_cloud: mpsc::Receiver<Message>, from_cloud: mpsc::Sender<Message>) {
        let mut backoff = BACKOFF_MIN;

        loop {
            match self.connected(&mut to_cloud, &from_cloud).await {
                Outcome::LocalShutdown => {
                    tracing::debug!("relay stopping: the device session ended");
                    return;
                }
                Outcome::Disconnected(reason) => {
                    tracing::warn!(
                        reason = %reason,
                        retry_in_s = backoff.as_secs(),
                        "cloud relay disconnected; local operation is unaffected"
                    );
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
        }
    }

    /// One connection to the cloud, from CONNECT to failure.
    async fn connected(&self, to_cloud: &mut mpsc::Receiver<Message>, from_cloud: &mpsc::Sender<Message>) -> Outcome {
        let mut stream = match self.handshake().await {
            Ok(stream) => stream,
            Err(reason) => return Outcome::Disconnected(reason),
        };

        if let Err(reason) = self.subscribe(&mut stream).await {
            return Outcome::Disconnected(reason);
        }

        self.pump(&mut stream, to_cloud, from_cloud).await
    }

    /// TCP, TLS, CONNECT, CONNACK.
    async fn handshake(&self) -> Result<PacketStream<TlsStream<TcpStream>>, String> {
        let address = self.config.address();
        tracing::info!(cloud = %address, "connecting the cloud relay");

        let tcp = TcpStream::connect(&address)
            .await
            .map_err(|error| format!("tcp connect: {error}"))?;
        drop(tcp.set_nodelay(true));

        let connector = self.tls.connector();
        let tls = connector
            .connect(self.server_name.clone(), tcp)
            .await
            .map_err(|error| format!("tls: {error}"))?;

        let mut stream = PacketStream::new(tls, MAX_PACKET_LEN);
        stream
            .send(&self.upstream_connect())
            .await
            .map_err(|error| format!("connect: {error}"))?;

        match tokio::time::timeout(CONNECT_TIMEOUT, stream.next_packet()).await {
            Ok(Ok(Some(Packet::ConnAck { code: 0, .. }))) => {
                tracing::info!(cloud = %address, "cloud relay connected");
                Ok(stream)
            }
            Ok(Ok(Some(Packet::ConnAck { code, .. }))) => Err(format!("cloud refused the connection, code {code}")),
            Ok(Ok(other)) => Err(format!(
                "expected CONNACK, got {}",
                other.map_or("nothing", |packet| packet.kind())
            )),
            Ok(Err(error)) => Err(format!("read: {error}")),
            Err(_) => Err("no CONNACK before the timeout".to_owned()),
        }
    }

    /// The CONNECT to send upstream: the device's own identity, not ours.
    fn upstream_connect(&self) -> Packet {
        Packet::Connect(Connect {
            protocol_level: PROTOCOL_LEVEL,
            client_id: self.device_id.clone(),
            username: Some(self.device_id.clone()),
            password: Some(DEVICE_PASSWORD.to_vec()),
            keepalive: KEEPALIVE_SECS,
            clean_session: false,
            // The device sets no will, so neither does the relay: it connects upstream *as* the device,
            // and a will the genuine device never sends would be a difference the cloud could see.
            will: None,
        })
    }

    /// Subscribe to both command topic forms.
    ///
    /// The device subscribes to `s/33/<serial>`, yet every observed cloud command arrived on
    /// `s/<serial>`. Since the relationship between the two was never established, subscribing to both is
    /// the only way to be sure a command is not missed.
    async fn subscribe(&self, stream: &mut PacketStream<TlsStream<TcpStream>>) -> Result<(), String> {
        let subscribe = Packet::Subscribe(Subscribe {
            packet_id: 1,
            filters: vec![
                (format!("s/33/{}", self.device_id), 1),
                (format!("s/{}", self.device_id), 1),
            ],
        });
        stream
            .send(&subscribe)
            .await
            .map_err(|error| format!("subscribe: {error}"))
    }

    /// Move messages in both directions until something fails.
    async fn pump(
        &self,
        stream: &mut PacketStream<TlsStream<TcpStream>>,
        to_cloud: &mut mpsc::Receiver<Message>,
        from_cloud: &mpsc::Sender<Message>,
    ) -> Outcome {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick completes immediately

        let mut packet_id: u16 = 2;

        loop {
            tokio::select! {
                outgoing = to_cloud.recv() => {
                    let Some(message) = outgoing else {
                        // The session dropped its handle. Say goodbye properly so the cloud does not hold
                        // a half-open session for this serial.
                        drop(stream.send(&Packet::Disconnect).await);
                        return Outcome::LocalShutdown;
                    };

                    let publish = self.uplink_publish(message, &mut packet_id);
                    if let Err(error) = stream.send(&publish).await {
                        return Outcome::Disconnected(format!("write: {error}"));
                    }
                }

                incoming = stream.next_packet() => {
                    match incoming {
                        Ok(Some(Packet::Publish(publish))) => {
                            if let Some(outcome) = self.handle_downlink(stream, publish, from_cloud).await {
                                return outcome;
                            }
                        }
                        Ok(Some(Packet::PingResp | Packet::PubAck { .. } | Packet::SubAck { .. })) => {}
                        Ok(Some(Packet::Disconnect)) => {
                            return Outcome::Disconnected("cloud sent DISCONNECT".to_owned());
                        }
                        Ok(Some(other)) => {
                            tracing::debug!(kind = other.kind(), "ignoring an unexpected packet from the cloud");
                        }
                        Ok(None) => return Outcome::Disconnected("cloud closed the connection".to_owned()),
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

    /// Turn a queued message into a publish for the cloud.
    fn uplink_publish(&self, message: Message, packet_id: &mut u16) -> Packet {
        let topic = if message.topic.is_empty() {
            format!("c/33/{}", self.device_id)
        } else {
            message.topic
        };

        let id = if message.qos == QoS::AtMostOnce {
            None
        } else {
            *packet_id = packet_id.checked_add(1).unwrap_or(2);
            Some(*packet_id)
        };

        Packet::Publish(Publish {
            topic,
            qos: message.qos,
            retain: false,
            dup: false,
            packet_id: id,
            payload: message.payload,
        })
    }

    /// Acknowledge a cloud publish and pass it to the session.
    ///
    /// Returns `Some` only if the connection has to end.
    async fn handle_downlink(
        &self,
        stream: &mut PacketStream<TlsStream<TcpStream>>,
        publish: Publish,
        from_cloud: &mpsc::Sender<Message>,
    ) -> Option<Outcome> {
        tracing::info!(
            topic = %publish.topic,
            len = publish.payload.len(),
            qos = %publish.qos,
            "cloud sent a message for the device"
        );

        // Acknowledge here rather than waiting for the device: hop by hop, so a slow device cannot make
        // the cloud retransmit.
        if let (QoS::AtLeastOnce, Some(id)) = (publish.qos, publish.packet_id)
            && let Err(error) = stream.send(&Packet::PubAck { packet_id: id }).await
        {
            return Some(Outcome::Disconnected(format!("puback: {error}")));
        }

        let message = Message {
            topic: publish.topic,
            qos: publish.qos,
            payload: publish.payload,
        };
        if from_cloud.try_send(message).is_err() {
            tracing::warn!("dropped a cloud message: the device session is not keeping up");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_HOST, DEFAULT_PORT, DEVICE_PASSWORD, KEEPALIVE_SECS, Message, Relay, RelayTask};
    use crate::driver::upstream::{Endpoint, Relay as _, Target};
    use crate::mqtt::{Packet, QoS, Trust};

    const SERIAL: &str = "0EXAMPLE00000001";

    /// The endpoint a Growatt device dials, which used to be this module's `Default`.
    fn growatt_endpoint() -> Endpoint {
        Endpoint {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
        }
    }

    /// A relay configuration pointing at `endpoint`, trusting whatever the environment says.
    fn relay_to(endpoint: Endpoint) -> Target {
        Target {
            endpoint,
            tls: Trust::BuiltIn.client_tls().expect("the shipped roots load"),
        }
    }

    #[test]
    fn defaults_point_at_the_vendor_endpoint() {
        let config = growatt_endpoint();
        assert_eq!(config.host, DEFAULT_HOST);
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.address(), "mqtt.growatt.com:7006");
    }

    #[test]
    fn an_unusable_host_is_refused_before_anything_is_spawned() {
        let config = Endpoint {
            host: "not a host name".to_owned(),
            port: 7006,
        };
        assert!(config.server_name().is_err());
        assert!(Relay::start(SERIAL, relay_to(config)).is_err());
    }

    #[test]
    fn the_upstream_connect_impersonates_the_device() {
        // The relay's identity upstream must be the device's, not its own.
        let task = RelayTask::new(SERIAL, relay_to(growatt_endpoint())).expect("valid config");
        let wire = task.upstream_connect().encode().expect("encode");

        let (decoded, _) = Packet::decode(&wire).expect("decode").expect("complete");
        match decoded {
            Packet::Connect(parsed) => {
                assert_eq!(parsed.client_id, SERIAL);
                assert_eq!(parsed.username.as_deref(), Some(SERIAL));
                assert_eq!(parsed.password.as_deref(), Some(DEVICE_PASSWORD));
                assert_eq!(parsed.keepalive, KEEPALIVE_SECS);
                assert!(!parsed.clean_session, "the device does not set clean session");
            }
            other => panic!("expected CONNECT, got {}", other.kind()),
        }

        // Flags octet: username + password, clean session clear — the same 0xC0 the device sends.
        assert_eq!(wire.get(2 + 2 + 4 + 1).copied(), Some(0xC0));
    }

    #[test]
    fn an_uplink_publish_uses_the_device_topic_and_a_fresh_packet_id() {
        let task = RelayTask::new(SERIAL, relay_to(growatt_endpoint())).expect("valid config");
        let mut packet_id = 2;

        match task.uplink_publish(Message::uplink(vec![1, 2, 3], QoS::AtLeastOnce), &mut packet_id) {
            Packet::Publish(publish) => {
                assert_eq!(publish.topic, format!("c/33/{SERIAL}"));
                assert_eq!(publish.qos, QoS::AtLeastOnce);
                assert_eq!(publish.packet_id, Some(3));
                assert_eq!(publish.payload, vec![1, 2, 3]);
            }
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }

        // A QoS-0 message needs no identifier, and must not consume one.
        match task.uplink_publish(Message::uplink(vec![4], QoS::AtMostOnce), &mut packet_id) {
            Packet::Publish(publish) => assert_eq!(publish.packet_id, None),
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }
        assert_eq!(packet_id, 3, "QoS 0 should not have advanced the identifier");
    }

    #[test]
    fn an_explicit_topic_is_preserved() {
        let task = RelayTask::new(SERIAL, relay_to(growatt_endpoint())).expect("valid config");
        let mut packet_id = 2;
        let message = Message {
            topic: "c/33/other".to_owned(),
            qos: QoS::AtLeastOnce,
            payload: vec![],
        };
        match task.uplink_publish(message, &mut packet_id) {
            Packet::Publish(publish) => assert_eq!(publish.topic, "c/33/other"),
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }
    }

    #[tokio::test]
    async fn a_full_queue_drops_rather_than_blocks() {
        // The property that keeps the cloud from ever delaying the device: forwarding never waits.
        let config = Endpoint {
            // A name that resolves nowhere, so the task cannot drain the queue.
            host: "cloud.invalid".to_owned(),
            port: 7006,
        };
        let mut relay = Relay::start(SERIAL, relay_to(config)).expect("valid host name");

        let mut queued = 0u64;
        for _ in 0..super::QUEUE_DEPTH.saturating_mul(4) {
            if relay.try_forward(Message::uplink(vec![0u8; 585], QoS::AtLeastOnce)) {
                queued = queued.saturating_add(1);
            }
        }

        assert!(queued <= super::QUEUE_DEPTH.saturating_add(1) as u64, "queued {queued}");
        assert!(relay.dropped() > 0, "some messages should have been dropped");
    }
}
