//! The MQTT server the device connects to.
//!
//! Not a broker. One client, nine packet types, no retained messages, no wildcards, no multi-client
//! routing, no QoS 2, no session state across restarts. What it must get right is small and specific:
//!
//! 1. **Grant QoS 1 in SUBACK.** Granting 0 produces a device that connects, subscribes, looks
//!    entirely healthy and then publishes nothing at all, indefinitely.
//! 2. **PUBACK every QoS-1 uplink**, promptly.
//! 3. **Answer PINGREQ**, or the device tears the connection down on its 420-second keepalive.
//!
//! The device publishes without being asked, roughly 0.6 s after subscribing. It is not poll-driven,
//! so a server that sends nothing beyond SUBACK still receives telemetry.

use core::time::Duration;

use snafu::{ResultExt, Snafu};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;

use crate::growatt::cloud::{CloudConfig, Message as CloudMessage, Relay};
use crate::growatt::v7::decode::{FromFrame, Telemetry};
use crate::growatt::v7::encode::{Command, EncodeError};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::{Codec, peek_version};
use crate::model::{Hex, Timestamp};
use crate::mqtt::{Packet, PacketStream, Publish, QoS, StreamError};
use crate::server::clock::{Clock, Skew};
use crate::{TARGET_VALUES, TARGET_WIRE};

/// Keepalive the device asks for: seven minutes.
pub const DEVICE_KEEPALIVE: Duration = Duration::from_mins(7);

/// How long to wait for a read before assuming the connection is dead.
///
/// One and a half keepalive intervals. Shorter risks dropping a healthy device that is merely quiet;
/// much longer leaves a dead socket occupying the single-client slot.
pub const READ_TIMEOUT: Duration = Duration::from_secs(630);

/// Largest packet accepted. The biggest the device sends is the 839-octet settings snapshot plus its
/// MQTT framing, so this is generous while still bounding memory against a confused peer.
pub const MAX_PACKET_LEN: usize = 64 * 1024;

/// QoS granted in every SUBACK. See the module documentation.
pub const GRANTED_QOS: u8 = 0x01;

/// How long after connect to send the server time push.
///
/// The vendor server waits about this long. Copying the delay is useful rather than merely faithful: by
/// then the device has published its first telemetry, so its own clock can be compared with ours before
/// ours replaces it.
pub const TIME_PUSH_DELAY: Duration = Duration::from_millis(4_500);

/// Why a session ended.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum SessionError {
    /// The framed connection failed: transport error, malformed packet, or an oversized one.
    #[snafu(display("connection failed"))]
    Stream {
        /// What the stream said.
        source: StreamError,
    },

    /// Nothing arrived within [`READ_TIMEOUT`].
    #[snafu(display("no packet for {}s", READ_TIMEOUT.as_secs()))]
    Idle,

    /// The peer sent something before CONNECT.
    #[snafu(display("first packet was {kind}, not CONNECT"))]
    NotConnectFirst {
        /// What arrived instead.
        kind: &'static str,
    },

    /// An unsupported protocol level.
    #[snafu(display("client offered MQTT protocol level {level}, this server speaks 4"))]
    UnsupportedLevel {
        /// Level offered.
        level: u8,
    },

    /// A frame this server wanted to send could not be built.
    #[snafu(display("could not build a frame to send"))]
    Encode {
        /// What the encoder said.
        source: EncodeError,
    },
}

/// What a session did, reported when it ends.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// Frames received and successfully parsed.
    pub frames: u64,
    /// Telemetry frames decoded.
    pub telemetry: u64,
    /// Frames that failed to parse.
    pub rejected: u64,
    /// Frames of a message type this codec does not decode.
    pub undecoded: u64,
    /// Keepalive exchanges.
    pub pings: u64,
    /// Messages the cloud sent for the device, when relaying.
    pub relay_received: u64,
    /// Frames that could not be handed to the cloud because it was not keeping up.
    pub relay_dropped: u64,
}

/// A device session over an established, already-encrypted stream.
///
/// Generic over the stream so the whole state machine can be driven over an in-memory duplex in
/// tests, with no TLS, no sockets and no device.
#[derive(Debug)]
pub struct Session<S> {
    stream: PacketStream<S>,
    device_id: Option<String>,
    subscribed: bool,
    stats: SessionStats,
    clock: Clock,
    send_time_push: bool,
    time_push_due: Option<Instant>,
    next_packet_id: u16,
    device_time: Option<Timestamp>,
    warned_about_skew: bool,
    cloud: Option<CloudConfig>,
    relay: Option<Relay>,
}

impl<S> Session<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a connected stream, using the host's local clock for the time push.
    pub fn new(stream: S) -> Self {
        Self::with_clock(stream, Clock::system())
    }

    /// Wrap a connected stream with an explicit clock.
    ///
    /// Tests pass a fixed clock; nothing else should need this.
    pub fn with_clock(stream: S, clock: Clock) -> Self {
        Self {
            stream: PacketStream::new(stream, MAX_PACKET_LEN),
            device_id: None,
            subscribed: false,
            stats: SessionStats::default(),
            clock,
            send_time_push: true,
            time_push_due: None,
            next_packet_id: 1,
            device_time: None,
            warned_about_skew: false,
            cloud: None,
            relay: None,
        }
    }

    /// Whether to send the time push after connect. On by default, because the vendor server does.
    #[must_use]
    pub const fn with_time_push(mut self, enabled: bool) -> Self {
        self.send_time_push = enabled;
        self
    }

    /// Relay this session's traffic to the vendor cloud.
    ///
    /// The relay cannot start until the device's serial is known, so this stores the configuration and the
    /// connection is made from CONNECT.
    #[must_use]
    pub fn with_cloud(mut self, cloud: Option<CloudConfig>) -> Self {
        self.cloud = cloud;
        self
    }

    /// The device serial, once CONNECT has been seen.
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// Counters for this session.
    pub const fn stats(&self) -> SessionStats {
        self.stats
    }

    /// The most recent wall-clock time the device reported, if any.
    pub const fn device_time(&self) -> Option<Timestamp> {
        self.device_time
    }

    /// Run until the peer disconnects or something fails.
    ///
    /// # Errors
    ///
    /// [`SessionError`] describing why the session ended. A clean DISCONNECT, or the peer closing the
    /// socket, is `Ok`.
    #[tracing::instrument(skip(self), fields(device_id))]
    pub async fn run(&mut self) -> Result<SessionStats, SessionError> {
        loop {
            let packet = match self.wait().await? {
                Woke::TimePushDue => {
                    self.time_push_due = None;
                    self.push_time().await?;
                    continue;
                }
                Woke::FromCloud(Some(message)) => {
                    self.forward_to_device(message).await?;
                    continue;
                }
                Woke::FromCloud(None) => {
                    // Not a cloud outage — those are retried internally and never reach here. The relay
                    // task itself is gone, and a relay is built per session, so ending this session is
                    // what recreates it: the device reconnects within a couple of seconds and gets a fresh
                    // one. Cheaper and more surgical than taking the process down, and local operation is
                    // interrupted only for the length of a reconnect.
                    tracing::warn!(
                        ?self.stats,
                        "the cloud relay stopped; ending the session so a reconnect rebuilds it"
                    );
                    return Ok(self.stats);
                }
                Woke::Packet(None) => {
                    tracing::info!(?self.stats, "peer closed the connection");
                    return Ok(self.stats);
                }
                Woke::Packet(Some(packet)) => packet,
            };

            tracing::trace!(kind = packet.kind(), "received");

            match packet {
                Packet::Connect(connect) => {
                    if connect.protocol_level != crate::mqtt::PROTOCOL_LEVEL {
                        return Err(SessionError::UnsupportedLevel {
                            level: connect.protocol_level,
                        });
                    }
                    tracing::Span::current().record("device_id", &connect.client_id);
                    tracing::info!(
                        client_id = %connect.client_id,
                        keepalive_s = connect.keepalive,
                        clean_session = connect.clean_session,
                        username = connect.username.as_deref().unwrap_or("<none>"),
                        // Deliberately not logging the password. It is a known constant, not a secret,
                        // but a log that habitually prints credentials is a bad habit to establish.
                        "device connected"
                    );
                    self.device_id = Some(connect.client_id);
                    self.send(&Packet::ConnAck {
                        session_present: false,
                        code: 0,
                    })
                    .await?;

                    // The relay connects as the device, so it cannot start until the serial is known.
                    self.start_relay();

                    if self.send_time_push {
                        // The vendor server sends its push about 4.5 s after connect. Matching that
                        // delay rather than firing immediately is not superstition: by then the device
                        // has published its first telemetry, so its own clock is known and can be
                        // cross-checked before ours is imposed on it.
                        self.time_push_due = Instant::now().checked_add(TIME_PUSH_DELAY);
                    }
                }

                Packet::Subscribe(subscribe) => {
                    // One granted octet per requested filter, always QoS 1.
                    let granted = vec![GRANTED_QOS; subscribe.filters.len()];
                    for (filter, requested) in &subscribe.filters {
                        tracing::info!(filter, requested, granted = GRANTED_QOS, "subscription");
                    }
                    self.subscribed = true;
                    self.send(&Packet::SubAck {
                        packet_id: subscribe.packet_id,
                        granted,
                    })
                    .await?;
                }

                Packet::Publish(publish) => {
                    // Acknowledge first, then decode. The device is waiting for the PUBACK, and a
                    // decode problem must not delay or prevent it — a missing PUBACK stops telemetry,
                    // an undecodable frame merely loses one reading.
                    if let (QoS::AtLeastOnce, Some(packet_id)) = (publish.qos, publish.packet_id) {
                        self.send(&Packet::PubAck { packet_id }).await?;
                    }

                    // Relay before decoding, and regardless of whether decoding succeeds: the cloud
                    // understands frames this build does not, so what reaches Growatt must not depend on
                    // what this program can parse.
                    self.forward_to_cloud(&publish);
                    self.handle_frame(&publish.topic, &publish.payload);
                }

                Packet::PingReq => {
                    self.stats.pings = self.stats.pings.saturating_add(1);
                    tracing::debug!(count = self.stats.pings, "keepalive");
                    self.send(&Packet::PingResp).await?;
                }

                Packet::Disconnect => {
                    tracing::info!(?self.stats, "device disconnected cleanly");
                    return Ok(self.stats);
                }

                // The device acknowledging something this server published.
                Packet::PubAck { packet_id } => {
                    tracing::debug!(packet_id, "device acknowledged our publish");
                }

                // Server-to-device types are refused by the codec, so these are unreachable in
                // practice. Handled rather than ignored so the match stays exhaustive.
                Packet::ConnAck { .. } | Packet::SubAck { .. } | Packet::PingResp => {
                    tracing::warn!(kind = packet.kind(), "ignoring a server-to-device packet");
                }
            }
        }
    }

    /// Parse and log one protocol frame from a PUBLISH payload.
    fn handle_frame(&mut self, topic: &str, payload: &[u8]) {
        tracing::trace!(
            target: TARGET_WIRE,
            direction = "rx",
            topic,
            len = payload.len(),
            "{}",
            Hex(payload)
        );

        // Discover the generation before committing to a parser, so an unimplemented one is reported
        // as unsupported rather than as corruption.
        match peek_version(payload) {
            Some(version) if Codec::for_version(version).is_some() => {}
            Some(version) => {
                self.stats.rejected = self.stats.rejected.saturating_add(1);
                tracing::warn!(
                    %version,
                    len = payload.len(),
                    "unsupported protocol generation; ignoring the frame"
                );
                return;
            }
            None => {
                self.stats.rejected = self.stats.rejected.saturating_add(1);
                tracing::warn!(len = payload.len(), "payload too short to be a frame");
                return;
            }
        }

        let frame = match Frame::parse(payload) {
            Ok(frame) => frame,
            Err(error) => {
                self.stats.rejected = self.stats.rejected.saturating_add(1);
                // At warn with a hex dump rather than dropped: this is how the next unknown gets
                // characterised.
                tracing::warn!(
                    %error,
                    len = payload.len(),
                    dump = %Hex(payload),
                    "rejected a frame"
                );
                return;
            }
        };

        self.stats.frames = self.stats.frames.saturating_add(1);
        let message_type = frame.message_type();

        match message_type {
            MessageType::Telemetry => match Telemetry::from_frame(&frame) {
                Ok(telemetry) => {
                    self.stats.telemetry = self.stats.telemetry.saturating_add(1);
                    // Kept as the only independent reference for our own clock. A zero timestamp is
                    // reported occasionally and must not overwrite a good reading with nothing.
                    if let Some(stamp) = telemetry.timestamp.filter(|t| t.is_plausible()) {
                        self.device_time = Some(stamp);
                    }
                    self.log_telemetry(&telemetry);
                }
                Err(error) => {
                    self.stats.rejected = self.stats.rejected.saturating_add(1);
                    tracing::warn!(%error, "could not decode telemetry");
                }
            },

            // Known message types this codec cannot decode yet. Counted and named so their arrival is
            // visible rather than silent.
            MessageType::SettingsSnapshot
            | MessageType::IdentityReport
            | MessageType::ExtendedTelemetry
            | MessageType::ReadSingleRegister
            | MessageType::WriteSingleRegister
            | MessageType::WriteRegisterRange
            | MessageType::TimePush => {
                self.stats.undecoded = self.stats.undecoded.saturating_add(1);
                tracing::info!(
                    %message_type,
                    len = frame.wire_len(),
                    "received a frame this build does not decode yet"
                );
            }

            MessageType::Unrecognised { address, function } => {
                self.stats.undecoded = self.stats.undecoded.saturating_add(1);
                tracing::warn!(
                    address = format_args!("{address:#04x}"),
                    function = format_args!("{function:#04x}"),
                    len = frame.wire_len(),
                    dump = %Hex(payload),
                    "unrecognised message type"
                );
            }
        }
    }

    /// Publish the server's wall-clock time to the device.
    ///
    /// This is the only message the vendor server sends unprompted, and the only reason to send it is
    /// to behave like the server being replaced. The device maintains its own clock and kept working
    /// normally when the push was absent — but its clock is what drives time-slot scheduling, so
    /// leaving it to drift indefinitely is not a plan either.
    ///
    /// Sent on `s/<serial>` at QoS 1, matching the capture. The device's own subscription is
    /// `s/33/<serial>`, yet every observed server command arrived on `s/<serial>` and the device acted
    /// on all of them; the relationship between the two forms was never determined, so this follows
    /// what was observed rather than what would be tidy.
    async fn push_time(&mut self) -> Result<(), SessionError> {
        let Some(device_id) = self.device_id.clone() else {
            // Nothing to address it to. Cannot happen: the push is only scheduled from CONNECT.
            return Ok(());
        };

        let now = self.clock.now();
        self.check_clock_against_device(now);

        let command = Command::time_push(now).context(EncodeSnafu)?;
        let frame = command.to_frame(&device_id).context(EncodeSnafu)?;
        let wire = frame.to_wire();

        tracing::trace!(
            target: TARGET_WIRE,
            direction = "tx",
            len = wire.len(),
            "{}",
            Hex(&wire)
        );

        let packet_id = self.take_packet_id();
        tracing::info!(time = %now, packet_id, "pushing server time");

        self.send(&Packet::Publish(Publish {
            topic: format!("s/{device_id}"),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(packet_id),
            payload: wire,
        }))
        .await
    }

    /// Compare our clock with the device's and complain if they disagree materially.
    ///
    /// The device accepts whatever time it is told, so a misconfigured host would silently set it wrong
    /// and nothing in the protocol would reveal it. The device's own reported timestamp is the only
    /// independent reference available, so it is worth using before overwriting the thing it came from.
    fn check_clock_against_device(&mut self, ours: Timestamp) {
        let Some(theirs) = self.device_time else {
            return;
        };
        let Some(skew) = Skew::between(ours, theirs) else {
            return;
        };
        if !skew.is_significant() || self.warned_about_skew {
            return;
        }

        self.warned_about_skew = true;
        tracing::warn!(
            skew = %skew,
            ours = %ours,
            device = %theirs,
            "about to set the device clock, but it disagrees with ours: {}",
            skew.diagnosis()
        );
    }

    /// Next packet identifier for a QoS-1 publish, wrapping and never zero.
    fn take_packet_id(&mut self) -> u16 {
        let current = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.checked_add(1).unwrap_or(1);
        current
    }

    /// Wait for whichever of the three things happens first.
    ///
    /// Split out of the loop because each branch needs `self` afterwards, and doing the work inside a
    /// `select!` arm would hold a borrow taken by another arm's future. The branches that only need
    /// `Copy` state — the timer deadline — are set up before the `select!` for the same reason.
    async fn wait(&mut self) -> Result<Woke, SessionError> {
        match (self.time_push_due, self.relay.as_mut()) {
            (Some(due), Some(relay)) => Ok(tokio::select! {
                biased;
                () = tokio::time::sleep_until(due) => Woke::TimePushDue,
                message = relay.next_from_cloud() => Woke::FromCloud(message),
                packet = Self::read(&mut self.stream) => Woke::Packet(packet?),
            }),
            (Some(due), None) => Ok(tokio::select! {
                biased;
                () = tokio::time::sleep_until(due) => Woke::TimePushDue,
                packet = Self::read(&mut self.stream) => Woke::Packet(packet?),
            }),
            (None, Some(relay)) => Ok(tokio::select! {
                biased;
                message = relay.next_from_cloud() => Woke::FromCloud(message),
                packet = Self::read(&mut self.stream) => Woke::Packet(packet?),
            }),
            (None, None) => Ok(Woke::Packet(Self::read(&mut self.stream).await?)),
        }
    }

    /// Read the next packet from the device, bounded by the idle timeout.
    ///
    /// The framing itself belongs to [`PacketStream`]; what is added here is the timeout, which is a
    /// property of *this* session rather than of MQTT. Takes the stream rather than `&mut self` so it can
    /// sit in a `select!` beside branches that borrow other fields.
    async fn read(stream: &mut PacketStream<S>) -> Result<Option<Packet>, SessionError> {
        tokio::time::timeout(READ_TIMEOUT, stream.next_packet_from_device())
            .await
            .map_err(|_| SessionError::Idle)?
            .context(StreamSnafu)
    }

    /// Write one packet.
    async fn send(&mut self, packet: &Packet) -> Result<(), SessionError> {
        self.stream.send(packet).await.context(StreamSnafu)
    }

    /// Start the cloud relay, now that the serial is known.
    fn start_relay(&mut self) {
        let (Some(cloud), Some(device_id)) = (self.cloud.clone(), self.device_id.clone()) else {
            return;
        };
        match Relay::start(&device_id, cloud) {
            Ok(relay) => self.relay = Some(relay),
            // Not fatal. The device is served either way, which is the whole point of the relay being
            // optional.
            Err(error) => tracing::warn!(%error, "could not start the cloud relay; continuing without it"),
        }
    }

    /// Hand a frame the device published to the cloud, if relaying.
    fn forward_to_cloud(&mut self, publish: &Publish) {
        let Some(relay) = self.relay.as_mut() else {
            return;
        };
        let message = CloudMessage {
            topic: publish.topic.clone(),
            qos: publish.qos,
            payload: publish.payload.clone(),
        };
        if !relay.try_forward(message) {
            self.stats.relay_dropped = self.stats.relay_dropped.saturating_add(1);
            tracing::warn!(
                dropped = self.stats.relay_dropped,
                "dropped a frame for the cloud; it is not keeping up"
            );
        }
    }

    /// Pass a message the cloud sent on to the device.
    ///
    /// The frame is forwarded untouched. It is not decoded first, and deliberately so: a command this
    /// build cannot parse is still a command the device understands, and dropping it would make the relay
    /// less capable than the thing it relays.
    async fn forward_to_device(&mut self, message: CloudMessage) -> Result<(), SessionError> {
        self.stats.relay_received = self.stats.relay_received.saturating_add(1);

        tracing::trace!(
            target: TARGET_WIRE,
            direction = "cloud-rx",
            topic = %message.topic,
            len = message.payload.len(),
            "{}",
            Hex(&message.payload)
        );

        // Name what it is, when that is knowable, without depending on it.
        match Frame::parse(&message.payload) {
            Ok(frame) => tracing::info!(
                message_type = %frame.message_type(),
                topic = %message.topic,
                "relaying a cloud command to the device"
            ),
            Err(error) => tracing::warn!(
                %error,
                topic = %message.topic,
                len = message.payload.len(),
                "relaying a cloud message this build cannot parse"
            ),
        }

        let packet_id = if message.qos == QoS::AtMostOnce {
            None
        } else {
            Some(self.take_packet_id())
        };

        self.send(&Packet::Publish(Publish {
            topic: message.topic,
            qos: message.qos,
            retain: false,
            dup: false,
            packet_id,
            payload: message.payload,
        }))
        .await
    }

    /// Emit a decoded telemetry frame: a short line at `info`, every field at `trace`.
    fn log_telemetry(&self, telemetry: &Telemetry) {
        let field = |name: &str| telemetry.value(name).unwrap_or(f64::NAN);
        tracing::info!(
            timestamp = telemetry
                .timestamp
                .map_or_else(|| "none".to_owned(), |stamp| stamp.to_string()),
            pv_w = field("pv_power_total"),
            ac_w = field("ac_power"),
            battery_w = field("battery_charge_power"),
            soc_pct = field("battery_soc_total"),
            relayed = self.relay.is_some(),
            "telemetry"
        );

        if tracing::enabled!(target: TARGET_VALUES, tracing::Level::TRACE) {
            for reading in &telemetry.readings {
                tracing::trace!(
                    target: TARGET_VALUES,
                    register = reading.register.number(),
                    name = reading.name,
                    raw = reading.raw.get(),
                    value = %reading.value,
                    unit = reading.unit.symbol(),
                    "register"
                );
            }
        }
    }
}

/// What woke the session loop.
///
/// A small enum rather than acting inside the `select!` arms: doing the work there would need `self`
/// while the other arm's future still borrows it.
enum Woke {
    /// A packet arrived from the device, or it closed the connection.
    Packet(Option<Packet>),
    /// The time push became due.
    TimePushDue,
    /// The cloud sent something for the device, or the relay stopped.
    FromCloud(Option<CloudMessage>),
}

#[cfg(test)]
mod tests {
    use super::{Clock, Frame, GRANTED_QOS, MessageType, Session, TIME_PUSH_DELAY, Timestamp};
    use crate::mqtt::{Connect, Packet, Publish, QoS, Subscribe};

    const SERIAL: &str = "0EXAMPLE00000001";

    /// The CONNECT the device sends: flags 0xC0, keepalive 420, password `Growatt`.
    ///
    /// Built through the encoder rather than by hand. The reference octets are checked against a
    /// hand-built copy in the codec's own tests, which is the right place for that; here what matters is
    /// exercising the session, and hand-rolling packets in two places invites them to drift apart.
    fn connect_packet() -> Vec<u8> {
        Packet::Connect(Connect {
            protocol_level: crate::mqtt::PROTOCOL_LEVEL,
            client_id: SERIAL.to_owned(),
            username: Some(SERIAL.to_owned()),
            password: Some(b"Growatt".to_vec()),
            keepalive: 420,
            clean_session: false,
        })
        .encode()
        .expect("encode")
    }

    fn subscribe_packet() -> Vec<u8> {
        Packet::Subscribe(Subscribe {
            packet_id: 1,
            filters: vec![(format!("s/33/{SERIAL}"), 1)],
        })
        .encode()
        .expect("encode")
    }

    fn publish_packet(payload: &[u8], packet_id: u16) -> Vec<u8> {
        Packet::Publish(Publish {
            topic: format!("c/33/{SERIAL}"),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(packet_id),
            payload: payload.to_vec(),
        })
        .encode()
        .expect("encode")
    }

    /// Drive a scripted session over an in-memory duplex and return what the server wrote.
    ///
    /// Deliberately sequential, with no spawned task and no shutdown dance. The duplex buffers 64 KiB
    /// per direction, so the whole script can be written before anything reads it, and the replies fit
    /// comfortably while the session runs. Each script is terminated with a DISCONNECT, which ends the
    /// session through the protocol rather than by relying on how a given stream type signals
    /// end-of-file — the earlier version depended on that and deadlocked.
    async fn drive(script: &[u8]) -> (Vec<u8>, super::SessionStats) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut client, server) = tokio::io::duplex(64 * 1024);

        let mut full = script.to_vec();
        full.extend_from_slice(&[0xE0, 0x00]);
        client
            .write_all(&full)
            .await
            .expect("the duplex buffers the whole script");

        let mut session = Session::new(server);
        let stats = session.run().await.expect("session should end cleanly");
        // Dropping the server half is what gives the client its end-of-file below.
        drop(session);

        let mut replies = Vec::new();
        client.read_to_end(&mut replies).await.expect("read replies");
        (replies, stats)
    }

    /// Split a buffer of concatenated packets.
    fn packets(mut buf: &[u8]) -> Vec<Packet> {
        let mut out = Vec::new();
        while let Some((packet, used)) = Packet::decode(buf).expect("decode replies") {
            out.push(packet);
            buf = buf.get(used..).expect("advance");
        }
        out
    }

    #[tokio::test]
    async fn connack_then_suback_granting_qos_one() {
        let mut script = connect_packet();
        script.extend_from_slice(&subscribe_packet());
        let (replies, _) = drive(&script).await;
        let replies = packets(&replies);

        assert!(matches!(
            replies.first(),
            Some(Packet::ConnAck {
                session_present: false,
                code: 0
            })
        ));
        match replies.get(1) {
            Some(Packet::SubAck { packet_id, granted }) => {
                assert_eq!(*packet_id, 1);
                assert_eq!(granted, &vec![GRANTED_QOS]);
                assert_eq!(granted.first(), Some(&1), "SUBACK must grant QoS 1");
            }
            other => panic!("expected SUBACK, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_qos1_publish_is_acknowledged() {
        let frame = include_bytes!("../../tests/fixtures/telemetry-night-discharge.bin");
        let mut script = connect_packet();
        script.extend_from_slice(&subscribe_packet());
        script.extend_from_slice(&publish_packet(frame, 7));
        script.extend_from_slice(&publish_packet(frame, 8));

        let (replies, stats) = drive(&script).await;
        let acks: Vec<u16> = packets(&replies)
            .into_iter()
            .filter_map(|p| match p {
                Packet::PubAck { packet_id } => Some(packet_id),
                _ => None,
            })
            .collect();
        assert_eq!(acks, vec![7, 8], "both publishes must be acknowledged");
        assert_eq!(stats.telemetry, 2);
        assert_eq!(stats.rejected, 0);
    }

    #[tokio::test]
    async fn keepalive_is_answered() {
        let mut script = connect_packet();
        script.extend_from_slice(&[0xC0, 0x00]);
        let (replies, stats) = drive(&script).await;
        assert!(
            packets(&replies).contains(&Packet::PingResp),
            "PINGREQ must be answered or the device drops the connection"
        );
        assert_eq!(stats.pings, 1);
    }

    #[tokio::test]
    async fn a_corrupt_frame_is_still_acknowledged() {
        // The PUBACK must not depend on the payload being decodable: a missing acknowledgement stops
        // telemetry altogether, whereas an unparseable frame costs one reading.
        let mut script = connect_packet();
        script.extend_from_slice(&publish_packet(&[0xDE, 0xAD, 0xBE, 0xEF], 9));
        let (replies, stats) = drive(&script).await;
        assert!(packets(&replies).contains(&Packet::PubAck { packet_id: 9 }));
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.frames, 0);
    }

    #[tokio::test]
    async fn a_clean_disconnect_ends_the_session() {
        let mut script = connect_packet();
        script.extend_from_slice(&[0xE0, 0x00]);
        let (_, stats) = drive(&script).await;
        assert_eq!(stats.frames, 0);
    }

    /// A fixed clock, so the pushed timestamp is predictable.
    fn fixed_clock() -> Timestamp {
        Timestamp {
            year: 2026,
            month: 8,
            day: 6,
            hour: 23,
            minute: 43,
            second: 2,
        }
    }

    /// Drive a session with a fixed clock and time pushes enabled, letting the timer fire.
    async fn drive_with_time_push(script: &[u8], enabled: bool) -> Vec<Packet> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        client.write_all(script).await.expect("buffered");

        let mut session = Session::with_clock(server, Clock::from_fn(fixed_clock)).with_time_push(enabled);

        // Run until the session ends. The script has no DISCONNECT, so it ends on the read timeout —
        // which tokio's paused clock reaches instantly once the time push has fired.
        let run = tokio::spawn(async move {
            let outcome = session.run().await;
            drop(session);
            outcome
        });

        // With the test clock paused, tokio advances to the earliest pending timer whenever everything
        // is idle. Sleeping *longer* than the push delay is what makes the ordering deterministic: the
        // push timer is the earlier of the two, so it fires first and the DISCONNECT follows.
        tokio::time::sleep(TIME_PUSH_DELAY.saturating_add(core::time::Duration::from_millis(500))).await;
        client.write_all(&[0xE0, 0x00]).await.expect("send DISCONNECT");

        drop(run.await.expect("join"));

        let mut replies = Vec::new();
        client.read_to_end(&mut replies).await.expect("read replies");
        packets(&replies)
    }

    #[tokio::test(start_paused = true)]
    async fn the_time_push_goes_out_after_connect() {
        let replies = drive_with_time_push(&connect_packet(), true).await;

        let push = replies
            .iter()
            .find_map(|packet| match packet {
                Packet::Publish(publish) => Some(publish),
                _ => None,
            })
            .expect("a publish should have been sent");

        // Topic and QoS follow the capture: the device subscribes to s/33/<serial> but every observed
        // server command arrived on s/<serial>, and the time push specifically used QoS 1.
        assert_eq!(push.topic, format!("s/{SERIAL}"));
        assert_eq!(push.qos, QoS::AtLeastOnce);
        assert_eq!(push.packet_id, Some(1));

        // The payload is a real 0xFE18 frame carrying the fixed clock's time.
        let frame = Frame::parse(&push.payload).expect("the payload must be a valid frame");
        assert_eq!(frame.message_type(), MessageType::TimePush);
        assert_eq!(frame.wire_len(), 67);
        assert_eq!(frame.device_id(), SERIAL);
        let body = frame.body();
        assert_eq!(
            body.get(8..).map(String::from_utf8_lossy).as_deref(),
            Some("2026-08-06 23:43:02")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_time_push_when_disabled() {
        // What happens when relaying: the cloud is the clock authority and we must not be a second one.
        let replies = drive_with_time_push(&connect_packet(), false).await;
        assert!(
            !replies.iter().any(|packet| matches!(packet, Packet::Publish(_))),
            "nothing should be published when the time push is off"
        );
        // The rest of the session still works.
        assert!(replies.iter().any(|p| matches!(p, Packet::ConnAck { .. })));
    }

    #[tokio::test]
    async fn the_device_serial_is_learned_from_connect() {
        use tokio::io::AsyncWriteExt as _;

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut script = connect_packet();
        script.extend_from_slice(&[0xE0, 0x00]);
        client.write_all(&script).await.expect("buffered");

        let mut session = Session::new(server);
        session.run().await.expect("session");
        assert_eq!(session.device_id(), Some(SERIAL));
    }
}
