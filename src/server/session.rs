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
use std::collections::{BTreeMap, VecDeque};

use snafu::{ResultExt, Snafu};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::control::{
    Action as ControlAction, Outcome, QUEUE_DEPTH as CONTROL_QUEUE_DEPTH, Registration, Registry,
    Request as ControlRequest, SessionHandle, SettingView,
};
use crate::growatt::cloud::{CloudConfig, Message as CloudMessage, Relay};
use crate::growatt::v7::decode::{FromFrame, ReadResponse, Telemetry, WriteAck};
use crate::growatt::v7::encode::{Command, EncodeError};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::v7::registers::HoldingRegister;
use crate::growatt::{Codec, peek_version};
use crate::model::{Confidence, Hex, Raw, Register, Timestamp, Unit};
use crate::mqtt::{Packet, PacketStream, Publish, QoS, StreamError};
use crate::record::{Recorder, Stream as RecordStream};
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

/// How long to wait for a read to be answered before moving on.
///
/// The device answered a read in about 0.6 s. Ten seconds is generous enough that a busy device is not
/// skipped, and short enough that a resync of sixty registers cannot stall for minutes if one is ignored.
pub const READ_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Settings read back successfully.
    pub reads: u64,
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
    recorder: Option<Recorder>,
    slots: u16,
    /// Registers waiting to be read, each knowing why.
    ///
    /// One queue rather than two because only one read may be in flight — the device answers sequentially
    /// and responses identify themselves by register. Two queues would need a scheduler to choose between
    /// them, which is the same thing with more parts. Verification reads go to the front: a startup resync
    /// has up to an hour of slack, a caller holding an HTTP connection has twenty seconds.
    reads: VecDeque<PendingRead>,
    awaiting: Option<PendingRead>,
    /// A read decided on but not yet transmitted.
    ///
    /// The decision is made while handling a packet, which cannot await; the loop sends it immediately
    /// afterwards. Without this the read would have to be issued from a synchronous context.
    pending_read: Option<Register>,
    read_deadline: Option<Instant>,
    settings: BTreeMap<Register, Raw>,
    registry: Option<Registry>,
    /// Requests from the control API, once this session has registered itself.
    requests: Option<mpsc::Receiver<ControlRequest>>,
    /// Where to publish settings so the API can answer reads without device traffic.
    settings_out: Option<watch::Sender<Vec<SettingView>>>,
    /// Removes this session from the registry when dropped, including on the error paths.
    registration: Option<Registration>,
}

/// A register queued to be read, and why.
///
/// Carrying the purpose is what lets one queue serve both the startup read-back and one-off verifications:
/// the alternative was a side table keyed by register plus a flag, which is the same information stored
/// twice and able to disagree with itself.
#[derive(Debug)]
struct PendingRead {
    /// Which register, and how to render what comes back.
    entry: HoldingRegister,
    /// Why it is being read.
    purpose: ReadPurpose,
}

impl PendingRead {
    /// Whether this read belongs to the startup sequence.
    const fn is_resync(&self) -> bool {
        matches!(self.purpose, ReadPurpose::Resync)
    }

    /// What the caller asked the register to hold, if this is confirming a write.
    const fn requested(&self) -> Option<Raw> {
        match self.purpose {
            ReadPurpose::Verify { requested, .. } => requested,
            ReadPurpose::Resync => None,
        }
    }

    /// Take whoever is waiting on this read, if anyone is.
    fn take_reply(&mut self) -> Option<oneshot::Sender<Outcome>> {
        match &mut self.purpose {
            ReadPurpose::Verify { reply, .. } => reply.take(),
            ReadPurpose::Resync => None,
        }
    }
}

/// Why a register is being read.
#[derive(Debug)]
enum ReadPurpose {
    /// Part of the startup read-back, which announces itself once when the last of them lands.
    Resync,
    /// Confirming a write, or a caller asking directly.
    Verify {
        /// What the write asked for, if anything did. `None` means there is only something to learn.
        requested: Option<Raw>,
        /// Where to report the result.
        reply: Option<oneshot::Sender<Outcome>>,
    },
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
            recorder: None,
            slots: 1,

            awaiting: None,
            pending_read: None,
            read_deadline: None,
            settings: BTreeMap::new(),
            registry: None,
            requests: None,
            settings_out: None,
            registration: None,
            reads: VecDeque::new(),
        }
    }

    /// Announce this session to the control API once its device is known.
    #[must_use]
    pub fn with_registry(mut self, registry: Option<Registry>) -> Self {
        self.registry = registry;
        self
    }

    /// Record every frame this session handles.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Option<Recorder>) -> Self {
        self.recorder = recorder;
        self
    }

    /// How many schedule slots to read back at startup.
    #[must_use]
    pub const fn with_slots(mut self, slots: u16) -> Self {
        self.slots = slots;
        self
    }

    /// Settings learned by reading them back, keyed by register.
    pub const fn settings(&self) -> &BTreeMap<Register, Raw> {
        &self.settings
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
                Woke::ReadTimedOut => {
                    // One unanswered read must not stall the rest. Recorded and skipped: the value stays
                    // unknown, which is honest, rather than being inferred.
                    if let Some(mut read) = self.awaiting.take() {
                        tracing::warn!(
                            register = %read.entry.register,
                            name = read.entry.name,
                            timeout_s = READ_REPLY_TIMEOUT.as_secs(),
                            "no answer to a settings read; moving on"
                        );
                        // Anyone waiting on this one is told now, rather than left to time out separately.
                        if let Some(reply) = read.take_reply() {
                            drop(reply.send(Outcome::timed_out(read.entry.register, read.requested())));
                        }
                    }
                    self.start_next_read();
                    self.send_pending_read().await?;
                    continue;
                }
                Woke::Control(Some(request)) => {
                    self.handle_control(request).await?;
                    continue;
                }
                Woke::Control(None) => {
                    // The API stopped. Nothing local depends on it, so carry on serving the device.
                    tracing::warn!("the control API stopped; continuing without it");
                    self.requests = None;
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

            if self.handle_packet(packet).await? == Flow::Stop {
                return Ok(self.stats);
            }
        }
    }

    /// Act on one packet from the device.
    ///
    /// Split from [`Session::run`] so the loop is the loop and this is the protocol. They grow at different
    /// rates: the loop has been stable while this gains a case per message type implemented.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if replying fails or the peer is not speaking MQTT 3.1.1.
    async fn handle_packet(&mut self, packet: Packet) -> Result<Flow, SessionError> {
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

                // Both need the serial: the relay connects upstream as the device, and control routes are
                // scoped to it.
                self.start_relay();
                self.register();

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

                // Only now: a read is a publish the device has to be subscribed to receive.
                self.begin_resync();
                self.send_pending_read().await?;
            }

            Packet::Publish(publish) => {
                // Acknowledge first, then decode. The device is waiting for the PUBACK, and a
                // decode problem must not delay or prevent it — a missing PUBACK stops telemetry,
                // an undecodable frame merely loses one reading.
                if let (QoS::AtLeastOnce, Some(packet_id)) = (publish.qos, publish.packet_id) {
                    self.send(&Packet::PubAck { packet_id }).await?;
                }

                // Record and relay before decoding, and regardless of whether decoding succeeds: the
                // cloud understands frames this build does not, and a recording is worth most for
                // exactly the frames this build cannot yet read.
                self.record(RecordStream::Up, &publish.payload);
                self.forward_to_cloud(&publish);
                self.handle_frame(&publish.topic, &publish.payload);

                // A read response may have decided the next read; transmitting needs an await, which
                // frame handling cannot do.
                self.send_pending_read().await?;
            }

            Packet::PingReq => {
                self.stats.pings = self.stats.pings.saturating_add(1);
                tracing::debug!(count = self.stats.pings, "keepalive");
                self.send(&Packet::PingResp).await?;
            }

            Packet::Disconnect => {
                tracing::info!(?self.stats, "device disconnected cleanly");
                return Ok(Flow::Stop);
            }

            // The device acknowledging something this server published.
            Packet::PubAck { packet_id } => {
                tracing::debug!(packet_id, "device acknowledged our publish");
            }

            // Server-to-device types are refused by the codec, so these are unreachable in practice.
            // Handled rather than ignored so the match stays exhaustive.
            other @ (Packet::ConnAck { .. } | Packet::SubAck { .. } | Packet::PingResp) => {
                tracing::warn!(kind = other.kind(), "ignoring a server-to-device packet");
            }
        }

        Ok(Flow::Continue)
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

            MessageType::ReadSingleRegister => match ReadResponse::from_frame(&frame) {
                Ok(response) => {
                    self.stats.reads = self.stats.reads.saturating_add(1);
                    self.accept_read(response);
                }
                Err(error) => {
                    self.stats.rejected = self.stats.rejected.saturating_add(1);
                    tracing::warn!(%error, "could not decode a read response");
                }
            },

            MessageType::WriteSingleRegister | MessageType::WriteRegisterRange => self.accept_write_ack(&frame),

            // Known message types this codec cannot decode yet. Counted and named so their arrival is
            // visible rather than silent.
            MessageType::SettingsSnapshot
            | MessageType::IdentityReport
            | MessageType::ExtendedTelemetry
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

        // Recorded as `inject` rather than `down`: from the device's side the two are indistinguishable,
        // and when a write misbehaves the first question is whether this program sent what it thought.
        self.record(RecordStream::Inject, &wire);

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

    /// Wait for whichever thing happens first.
    ///
    /// Split out of the loop because each branch needs `self` afterwards, and doing the work inside a
    /// `select!` arm would hold a borrow taken by another arm's future.
    ///
    /// Every branch is always present. A timer that is not set, or a relay that is not configured, becomes
    /// a future that never resolves — so adding a timer adds one arm rather than doubling the arrangements
    /// this function has to spell out. The four arms borrow disjoint fields, which is what lets them sit
    /// together.
    async fn wait(&mut self) -> Result<Woke, SessionError> {
        Ok(tokio::select! {
            biased;
            () = Self::at(self.time_push_due) => Woke::TimePushDue,
            () = Self::at(self.read_deadline) => Woke::ReadTimedOut,
            request = Self::from_control(self.requests.as_mut()) => Woke::Control(request),
            message = Self::from_cloud(self.relay.as_mut()) => Woke::FromCloud(message),
            packet = Self::read(&mut self.stream) => Woke::Packet(packet?),
        })
    }

    /// The next control request, or never if this session has not registered.
    async fn from_control(requests: Option<&mut mpsc::Receiver<ControlRequest>>) -> Option<ControlRequest> {
        match requests {
            Some(requests) => requests.recv().await,
            None => core::future::pending().await,
        }
    }

    /// Resolve at a deadline, or never if there is none.
    async fn at(deadline: Option<Instant>) {
        match deadline {
            Some(at) => tokio::time::sleep_until(at).await,
            None => core::future::pending().await,
        }
    }

    /// The next message from the cloud, or never if there is no relay.
    async fn from_cloud(relay: Option<&mut Relay>) -> Option<CloudMessage> {
        match relay {
            Some(relay) => relay.next_from_cloud().await,
            None => core::future::pending().await,
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

    /// Queue every setting for reading back, and start the sequence.
    ///
    /// Switch positions never appear in periodic telemetry. Without this they are visible only in the hourly
    /// settings snapshot, so a server that restarts mid-hour would have nothing to say about them for up to
    /// an hour — or would guess, which is worse.
    fn begin_resync(&mut self) {
        for entry in HoldingRegister::resync_set(self.slots) {
            self.reads.push_back(PendingRead {
                entry,
                purpose: ReadPurpose::Resync,
            });
        }
        tracing::info!(
            registers = self.reads.len(),
            slots = self.slots,
            "reading settings back to resynchronise"
        );
        self.start_next_read();
    }

    /// Issue the next read, if any remain.
    ///
    /// One at a time on purpose. The device answered a read in about 0.6 s, and a burst of sixty would be
    /// a novel load on hardware that cannot be patched — for a sequence that only has to finish before the
    /// hourly snapshot would have arrived anyway. That is also why there is one queue rather than two: with
    /// a single read in flight, a second queue would only need a scheduler to choose between them.
    fn start_next_read(&mut self) {
        let Some(next) = self.reads.pop_front() else {
            self.awaiting = None;
            self.read_deadline = None;
            return;
        };

        self.pending_read = Some(next.entry.register);
        self.awaiting = Some(next);
        self.read_deadline = Instant::now().checked_add(READ_REPLY_TIMEOUT);
    }

    /// Whether a startup resync is still draining.
    fn resync_outstanding(&self) -> bool {
        self.awaiting.as_ref().is_some_and(PendingRead::is_resync) || self.reads.iter().any(PendingRead::is_resync)
    }

    /// Send the queued read request. Separate from [`Session::start_next_read`] because that is called from
    /// places that cannot await.
    async fn send_pending_read(&mut self) -> Result<(), SessionError> {
        let Some(register) = self.pending_read.take() else {
            return Ok(());
        };
        let Some(device_id) = self.device_id.clone() else {
            return Ok(());
        };

        let frame = Command::read(register).to_frame(&device_id).context(EncodeSnafu)?;
        let wire = frame.to_wire();
        self.record(RecordStream::Inject, &wire);

        tracing::debug!(register = %register, "reading a setting back");

        let packet_id = self.take_packet_id();
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

    /// Record a value the device reported, and move the sequence along.
    fn accept_read(&mut self, response: ReadResponse) {
        let expected = self.awaiting.as_ref().map(|read| read.entry.register);
        if expected != Some(response.register) {
            // Not what was asked for. Kept anyway — it is still a fact about the device — but the sequence
            // is not advanced by it, or a stray response would skip whichever register is outstanding.
            tracing::warn!(
                got = %response.register,
                expected = expected.map_or_else(|| "nothing".to_owned(), |register| register.to_string()),
                "unexpected read response"
            );
            self.settings.insert(response.register, response.raw);
            self.publish_settings();
            return;
        }

        let Some(read) = self.awaiting.take() else {
            return;
        };
        tracing::debug!(
            register = %response.register,
            name = read.entry.name,
            value = %read.entry.decode(response.raw),
            raw = response.raw.get(),
            "setting"
        );

        self.settings.insert(response.register, response.raw);
        self.publish_settings();
        self.settle(read, response.raw);
        self.start_next_read();
    }

    /// Note the device's answer to a write.
    ///
    /// Informative rather than authoritative: a range acknowledgement reports acceptance even for a value
    /// the device clamped, so the read-back still decides. A refusal is worth saying out loud, though — it
    /// is the one case where the device volunteers that it did not do as asked.
    fn accept_write_ack(&mut self, frame: &Frame) {
        match WriteAck::from_frame(frame) {
            Ok(ack) if ack.accepted() => tracing::debug!(
                start = %ack.start,
                end = %ack.end,
                value = ack.value.map_or_else(|| "none".to_owned(), |raw| raw.get().to_string()),
                "device acknowledged a write"
            ),
            Ok(ack) => tracing::warn!(
                start = %ack.start,
                end = %ack.end,
                status = ack.status,
                "device refused a write"
            ),
            Err(error) => {
                self.stats.rejected = self.stats.rejected.saturating_add(1);
                tracing::warn!(%error, "could not decode a write acknowledgement");
            }
        }
    }

    /// Finish one read according to why it was issued.
    fn settle(&self, read: PendingRead, stored: Raw) {
        let register = read.entry.register;
        match read.purpose {
            // The startup sequence announces itself only once, when the last of its reads lands. Sharing
            // the queue with verification reads is what makes carrying the purpose necessary: without it,
            // every one-off read looks like a resync completing.
            ReadPurpose::Resync => {
                if !self.resync_outstanding() {
                    self.finish_resync();
                }
            }

            ReadPurpose::Verify { requested, reply } => {
                let outcome = Outcome::read_back(register, requested, stored);

                if let Some(wanted) = requested {
                    if wanted == stored {
                        tracing::info!(
                            register = %register,
                            name = read.entry.name,
                            stored = stored.get(),
                            "write confirmed by read-back"
                        );
                    } else {
                        // Not an error: the device did what it was going to do. Reported because a write
                        // that stores something else is indistinguishable from success at the protocol
                        // level.
                        tracing::warn!(
                            register = %register,
                            name = read.entry.name,
                            requested = wanted.get(),
                            stored = stored.get(),
                            "the device did not store what was written"
                        );
                    }
                }

                if let Some(reply) = reply {
                    // A dropped receiver means the caller gave up; the value is still recorded.
                    drop(reply.send(outcome));
                }
            }
        }
    }

    /// Log what the resync learned, and publish it.
    fn finish_resync(&self) {
        self.publish_settings();
        let known = self.settings.len();
        tracing::info!(known, "settings resynchronised");

        for (register, raw) in &self.settings {
            let Some(entry) = HoldingRegister::lookup(*register) else {
                continue;
            };
            tracing::info!(
                register = %register,
                name = entry.name,
                value = %entry.decode(*raw),
                unit = entry.unit.symbol(),
                "setting"
            );
        }
    }

    /// Join the registry so the control API can find this session.
    ///
    /// Only once the serial is known: routes are device-scoped, and a session with no name has nothing to
    /// be addressed by.
    fn register(&mut self) {
        let (Some(registry), Some(device_id)) = (self.registry.clone(), self.device_id.clone()) else {
            return;
        };

        let (request_tx, request_rx) = mpsc::channel(CONTROL_QUEUE_DEPTH);
        let (settings_tx, settings_rx) = watch::channel(Vec::new());

        self.registration = Some(registry.register(
            &device_id,
            SessionHandle {
                requests: request_tx,
                settings: settings_rx,
            },
        ));
        self.requests = Some(request_rx);
        self.settings_out = Some(settings_tx);
        tracing::info!("registered with the control API");
    }

    /// Carry out one control request.
    ///
    /// Both kinds end the same way: a register is read off the device and the answer reports what it holds.
    /// A write is not confirmed by having been sent — range writes are acknowledged with the register range
    /// and nothing else, single-register writes are not acknowledged at all, and out-of-range values are
    /// clamped silently.
    async fn handle_control(&mut self, request: ControlRequest) -> Result<(), SessionError> {
        let ControlRequest { action, reply } = request;

        match action {
            ControlAction::Refresh(register) => {
                self.enqueue_verification(register, None, Some(reply));
            }
            ControlAction::Apply(command) => {
                let verify = command.registers_to_verify();
                self.transmit(&command).await?;

                if verify.is_empty() {
                    // Nothing to read back — a read or a time push. Answer immediately so the caller is not
                    // left waiting for a confirmation that will never come.
                    drop(reply.send(Outcome::read_back(Register(0), None, Raw(0))));
                    return Ok(());
                }

                // The reply belongs to the first register; the rest are learned but unreported. One request,
                // one answer, and the first entry is the one that was asked for.
                let mut reply = Some(reply);
                for (register, requested) in verify {
                    self.enqueue_verification(register, requested, reply.take());
                }
            }
        }

        self.send_pending_read().await
    }

    /// Queue a register to be read back, ahead of any startup resync still in progress.
    ///
    /// Ahead, because someone is waiting on this one: a resync has up to an hour of slack, a caller holding
    /// an HTTP connection has twenty seconds.
    fn enqueue_verification(
        &mut self,
        register: Register,
        requested: Option<Raw>,
        reply: Option<oneshot::Sender<Outcome>>,
    ) {
        let entry = HoldingRegister::lookup(register).unwrap_or_else(|| {
            // Readable but undocumented. Reading is harmless, and a value is still a fact; it simply has no
            // name or domain to render it with.
            HoldingRegister::range(
                register.number(),
                "unknown",
                0,
                u16::MAX,
                Unit::None,
                Confidence::Inferred,
            )
        });

        self.reads.push_front(PendingRead {
            entry,
            purpose: ReadPurpose::Verify { requested, reply },
        });
        if self.awaiting.is_none() {
            self.start_next_read();
        }
    }

    /// Send a command to the device.
    ///
    /// Writes go out at QoS 0 and the time push at QoS 1, matching what the vendor server was observed to
    /// do. Recorded as `inject`, because from the device's side our frames and the cloud's are
    /// indistinguishable and that is exactly what makes a misbehaving write hard to diagnose.
    async fn transmit(&mut self, command: &Command) -> Result<(), SessionError> {
        let Some(device_id) = self.device_id.clone() else {
            return Ok(());
        };

        let frame = command.to_frame(&device_id).context(EncodeSnafu)?;
        let wire = frame.to_wire();
        self.record(RecordStream::Inject, &wire);

        tracing::info!(
            message_type = %frame.message_type(),
            acknowledged = command.is_acknowledged(),
            "sending a command to the device"
        );
        tracing::trace!(
            target: TARGET_WIRE,
            direction = "tx",
            len = wire.len(),
            "{}",
            Hex(&wire)
        );

        let qos = if matches!(command, Command::TimePush { .. }) {
            QoS::AtLeastOnce
        } else {
            QoS::AtMostOnce
        };
        let packet_id = if qos == QoS::AtMostOnce {
            None
        } else {
            Some(self.take_packet_id())
        };

        self.send(&Packet::Publish(Publish {
            topic: format!("s/{device_id}"),
            qos,
            retain: false,
            dup: false,
            packet_id,
            payload: wire,
        }))
        .await
    }

    /// Publish the current settings for the control API to read.
    fn publish_settings(&self) {
        let Some(out) = self.settings_out.as_ref() else {
            return;
        };
        let views: Vec<SettingView> = self
            .settings
            .iter()
            .filter_map(|(register, raw)| SettingView::new(*register, *raw))
            .collect();
        drop(out.send(views));
    }

    /// Hand a frame to the recorder, if one is configured.
    ///
    /// Never fails and never waits. Recording is subordinate to serving the device, so a recorder that
    /// cannot keep up loses records rather than delaying the session.
    fn record(&self, stream: RecordStream, payload: &[u8]) {
        if let Some(recorder) = self.recorder.as_ref() {
            recorder.record(stream, payload);
        }
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
        self.record(RecordStream::Down, &message.payload);

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
/// Whether the session should keep going after handling a packet.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Flow {
    /// Carry on.
    Continue,
    /// The peer is finished; end the session cleanly.
    Stop,
}

/// What woke the session loop.
///
/// A small enum rather than acting inside the `select!` arms: doing the work there would need `self` while
/// the other arm's future still borrows it.
enum Woke {
    /// A packet arrived from the device, or it closed the connection.
    Packet(Option<Packet>),
    /// The time push became due.
    TimePushDue,
    /// A read went unanswered for too long.
    ReadTimedOut,
    /// The control API asked for something, or stopped.
    Control(Option<ControlRequest>),
    /// The cloud sent something for the device, or the relay stopped.
    FromCloud(Option<CloudMessage>),
}

#[cfg(test)]
mod tests {
    use super::{Clock, Frame, GRANTED_QOS, MessageType, Raw, Session, TIME_PUSH_DELAY, Timestamp};
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
    async fn a_read_is_issued_after_subscribing() {
        // Only after SUBACK: a read is a publish, and the device has to be subscribed to receive it.
        let mut script = connect_packet();
        script.extend_from_slice(&subscribe_packet());
        let (replies, _) = drive(&script).await;
        let replies = packets(&replies);

        let publish = replies
            .iter()
            .find_map(|packet| match packet {
                Packet::Publish(publish) => Some(publish),
                _ => None,
            })
            .expect("a read should have been published");

        let frame = Frame::parse(&publish.payload).expect("the read must be a valid frame");
        assert_eq!(frame.message_type(), MessageType::ReadSingleRegister);
        // The first entry of the resync set is charge_limit_upper, register 250.
        assert_eq!(frame.body().get(..2), Some([0x00, 0xFA].as_slice()));
        assert_eq!(frame.wire_len(), 44);

        // Nothing is published before the subscription; a read sent then would be lost.
        let (before, _) = drive(&connect_packet()).await;
        assert!(
            !packets(&before).iter().any(|p| matches!(p, Packet::Publish(_))),
            "no read should be issued before the device subscribes"
        );
    }

    /// A read response for one register, as the device would answer.
    fn read_response(register: u16, value: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&register.to_be_bytes());
        body.extend_from_slice(&register.to_be_bytes());
        body.extend_from_slice(&value.to_be_bytes());
        let frame = Frame::new(MessageType::ReadSingleRegister, SERIAL, &body).expect("build");
        publish_packet(&frame.to_wire(), 100)
    }

    #[tokio::test]
    async fn answering_a_read_advances_to_the_next_register() {
        let mut script = connect_packet();
        script.extend_from_slice(&subscribe_packet());
        // Answer the first two registers of the resync set: 250 then 251.
        script.extend_from_slice(&read_response(250, 100));
        script.extend_from_slice(&read_response(251, 5));

        let (replies, stats) = drive(&script).await;
        let reads: Vec<u16> = packets(&replies)
            .into_iter()
            .filter_map(|packet| match packet {
                Packet::Publish(publish) => Frame::parse(&publish.payload)
                    .ok()
                    .and_then(|frame| frame.u16_at(38).map(Raw::get)),
                _ => None,
            })
            .collect();

        // Three reads issued: the initial one, then one after each answer.
        assert_eq!(reads, vec![250, 251, 304], "each answer should trigger the next read");
        assert_eq!(stats.reads, 2);
        assert_eq!(stats.rejected, 0);
    }

    #[tokio::test]
    async fn a_stray_read_response_is_kept_but_does_not_advance_the_sequence() {
        let mut script = connect_packet();
        script.extend_from_slice(&subscribe_packet());
        // Answer a register that was not asked for.
        script.extend_from_slice(&read_response(327, 1));

        let (replies, stats) = drive(&script).await;
        let reads: Vec<u16> = packets(&replies)
            .into_iter()
            .filter_map(|packet| match packet {
                Packet::Publish(publish) => Frame::parse(&publish.payload)
                    .ok()
                    .and_then(|frame| frame.u16_at(38).map(Raw::get)),
                _ => None,
            })
            .collect();

        // Only the initial read. A stray answer must not skip the register still being awaited.
        assert_eq!(reads, vec![250]);
        assert_eq!(stats.reads, 1, "the value is still counted and kept");
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
