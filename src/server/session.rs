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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::growatt::v7::decode::{FromFrame, Telemetry};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::{Codec, peek_version};
use crate::server::mqtt::{CodecError, Packet, QoS};
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

/// Why a session ended.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum SessionError {
    /// The socket failed.
    #[snafu(display("connection i/o failed"))]
    Io {
        /// The underlying error.
        source: std::io::Error,
    },

    /// The peer sent octets that are not a valid packet.
    ///
    /// Fatal: once framing is lost there is no way to resynchronise on a stream.
    #[snafu(display("could not decode an MQTT packet"))]
    Codec {
        /// What the codec said.
        source: CodecError,
    },

    /// A packet larger than [`MAX_PACKET_LEN`].
    #[snafu(display("peer announced a {len}-octet packet, above the {MAX_PACKET_LEN} limit"))]
    PacketTooLarge {
        /// Length announced.
        len: usize,
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
}

/// A device session over an established, already-encrypted stream.
///
/// Generic over the stream so the whole state machine can be driven over an in-memory duplex in
/// tests, with no TLS, no sockets and no device.
#[derive(Debug)]
pub struct Session<S> {
    stream: S,
    buf: Vec<u8>,
    device_id: Option<String>,
    subscribed: bool,
    stats: SessionStats,
}

impl<S> Session<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a connected stream.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(1024),
            device_id: None,
            subscribed: false,
            stats: SessionStats::default(),
        }
    }

    /// The device serial, once CONNECT has been seen.
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// Counters for this session.
    pub const fn stats(&self) -> SessionStats {
        self.stats
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
            let Some(packet) = self.next_packet().await? else {
                tracing::info!(?self.stats, "peer closed the connection");
                return Ok(self.stats);
            };

            tracing::trace!(kind = packet.kind(), "received");

            match packet {
                Packet::Connect(connect) => {
                    if connect.protocol_level != crate::server::mqtt::PROTOCOL_LEVEL {
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
            hex(payload)
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
                    dump = %hex(payload),
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
                    log_telemetry(&telemetry);
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
                    dump = %hex(payload),
                    "unrecognised message type"
                );
            }
        }
    }

    /// Read octets until a whole packet is buffered, or the peer closes.
    async fn next_packet(&mut self) -> Result<Option<Packet>, SessionError> {
        loop {
            if let Some((packet, used)) = Packet::decode_from_device(&self.buf).context(CodecSnafu)? {
                self.buf.drain(..used);
                return Ok(Some(packet));
            }

            if self.buf.len() > MAX_PACKET_LEN {
                return Err(SessionError::PacketTooLarge { len: self.buf.len() });
            }

            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout(READ_TIMEOUT, self.stream.read(&mut chunk))
                .await
                .map_err(|_| SessionError::Idle)?
                .context(IoSnafu)?;

            if read == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(chunk.get(..read).unwrap_or_default());
        }
    }

    /// Write one packet.
    async fn send(&mut self, packet: &Packet) -> Result<(), SessionError> {
        let wire = packet.encode().context(CodecSnafu)?;
        tracing::trace!(kind = packet.kind(), len = wire.len(), "sending");
        self.stream.write_all(&wire).await.context(IoSnafu)?;
        self.stream.flush().await.context(IoSnafu)
    }
}

/// Emit a decoded telemetry frame: a short line at `info`, every field at `trace`.
fn log_telemetry(telemetry: &Telemetry) {
    let field = |name: &str| telemetry.value(name).unwrap_or(f64::NAN);
    tracing::info!(
        timestamp = telemetry
            .timestamp
            .map_or_else(|| "none".to_owned(), |stamp| stamp.to_string()),
        pv_w = field("pv_power_total"),
        ac_w = field("ac_power"),
        battery_w = field("battery_charge_power"),
        soc_pct = field("battery_soc_total"),
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

/// Hex-encode for a diagnostic dump.
///
/// Called inside the `trace!` argument list so it only runs when the level is enabled.
fn hex(data: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(data.len().saturating_mul(2));
    for byte in data {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{GRANTED_QOS, Session};
    use crate::server::mqtt::{Packet, Publish, QoS, write_varint};

    const SERIAL: &str = "0EXAMPLE00000001";

    /// Build the CONNECT the device sends.
    fn connect_packet() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x00, 0x04]);
        body.extend_from_slice(b"MQTT");
        body.push(0x04);
        body.push(0xC0);
        body.extend_from_slice(&420u16.to_be_bytes());
        for _ in 0..2 {
            body.extend_from_slice(&u16::try_from(SERIAL.len()).unwrap().to_be_bytes());
            body.extend_from_slice(SERIAL.as_bytes());
        }
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"Growatt");
        let mut packet = vec![0x10];
        write_varint(&mut packet, body.len());
        packet.extend_from_slice(&body);
        packet
    }

    fn subscribe_packet() -> Vec<u8> {
        let filter = format!("s/33/{SERIAL}");
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&u16::try_from(filter.len()).unwrap().to_be_bytes());
        body.extend_from_slice(filter.as_bytes());
        body.push(0x01);
        let mut packet = vec![0x82];
        write_varint(&mut packet, body.len());
        packet.extend_from_slice(&body);
        packet
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
