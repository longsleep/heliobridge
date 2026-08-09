//! MQTT 3.1.1 packet codec — the subset one device uses.
//!
//! Nine packet types, a fixed header, a varint length and length-prefixed strings. This is
//! deliberately hand-written rather than taken from a crate: the surface is small and fully
//! specified, every candidate crate is either unmaintained or a fresh personal fork, and this sits on
//! the most critical path in the program. When the device does something unexpected, the parser is
//! readable.
//!
//! The codec serves both roles this program plays. Facing the device it is a server; facing the vendor
//! cloud and the Home Assistant broker it is a client, which is why every packet type encodes and decodes
//! in both directions. Two features exist only for the client side — **retained messages** and **will
//! messages** — because Home Assistant discovery is retained and availability is a last will. The device
//! uses neither.
//!
//! What is **not** implemented: QoS 2, wildcard subscriptions, topic aliases, and session state across
//! reconnects. Nothing this program talks to needs them.

use core::fmt;

/// Protocol name in the CONNECT variable header.
pub const PROTOCOL_NAME: &str = "MQTT";

/// Protocol level for MQTT 3.1.1.
pub const PROTOCOL_LEVEL: u8 = 4;

/// Largest remaining-length value a 4-octet varint can express.
pub const MAX_REMAINING_LEN: usize = 268_435_455;

/// Something wrong with the octets of an MQTT packet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The remaining-length varint ran past four octets.
    #[error("remaining-length varint is longer than four octets")]
    MalformedLength,

    /// A packet type this server does not implement.
    #[error("packet type {kind} is not one this server implements")]
    UnsupportedType {
        /// The type nibble.
        kind: u8,
    },

    /// A string field was not valid UTF-8.
    #[error("{field} is not valid UTF-8")]
    NotUtf8 {
        /// Which field.
        field: &'static str,
    },

    /// The packet ended in the middle of a field.
    #[error("{kind} packet ended inside its {field} field")]
    Truncated {
        /// Packet type name.
        kind: &'static str,
        /// Field being read.
        field: &'static str,
    },

    /// A QoS value outside what this server handles.
    #[error("QoS {qos} is not supported; this server handles 0 and 1")]
    UnsupportedQoS {
        /// The value found.
        qos: u8,
    },

    /// The declared length exceeds what the varint can hold.
    #[error("remaining length {len} exceeds the {MAX_REMAINING_LEN}-octet maximum")]
    TooLong {
        /// The length asked for.
        len: usize,
    },
}

/// The CONNECT flags octet.
///
/// A named view of one byte whose bits are otherwise indistinguishable from each other. Six of the
/// eight are meaningful, and two of those only qualify a seventh: the will QoS and will-retain bits mean
/// nothing unless the will flag is set.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct ConnectFlags(u8);

impl ConnectFlags {
    /// Start a fresh session rather than resuming one.
    const CLEAN_SESSION: u8 = 0x02;
    /// A will topic and message follow the client identifier.
    const WILL: u8 = 0x04;
    /// The broker retains the will message.
    const WILL_RETAIN: u8 = 0x20;
    /// A password follows.
    const PASSWORD: u8 = 0x40;
    /// A username follows.
    const USERNAME: u8 = 0x80;
    /// Bits 3–4 hold the will's delivery guarantee.
    const WILL_QOS_SHIFT: u8 = 3;

    /// Read the octet as it arrived.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The octet to put on the wire.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The flags describing a given CONNECT.
    fn of(connect: &Connect) -> Self {
        let mut bits = 0x00u8;
        if connect.username.is_some() {
            bits |= Self::USERNAME;
        }
        if connect.password.is_some() {
            bits |= Self::PASSWORD;
        }
        if connect.clean_session {
            bits |= Self::CLEAN_SESSION;
        }
        if let Some(will) = connect.will.as_ref() {
            bits |= Self::WILL;
            bits |= will.qos.bits() << Self::WILL_QOS_SHIFT;
            if will.retain {
                bits |= Self::WILL_RETAIN;
            }
        }
        Self(bits)
    }

    /// Whether a username follows the will fields.
    pub const fn has_username(self) -> bool {
        self.0 & Self::USERNAME != 0
    }

    /// Whether a password follows the username.
    pub const fn has_password(self) -> bool {
        self.0 & Self::PASSWORD != 0
    }

    /// Whether a will topic and message follow the client identifier.
    pub const fn has_will(self) -> bool {
        self.0 & Self::WILL != 0
    }

    /// Whether the broker should retain the will message.
    pub const fn will_retain(self) -> bool {
        self.0 & Self::WILL_RETAIN != 0
    }

    /// Whether the client asked for a fresh session.
    pub const fn clean_session(self) -> bool {
        self.0 & Self::CLEAN_SESSION != 0
    }

    /// The will's delivery guarantee.
    ///
    /// # Errors
    ///
    /// [`CodecError::UnsupportedQoS`] if the two bits say 2 or 3.
    pub const fn will_qos(self) -> Result<QoS, CodecError> {
        QoS::from_bits((self.0 >> Self::WILL_QOS_SHIFT) & 0x03)
    }
}

/// The PUBLISH flags nibble, in the low four bits of the fixed header's first octet.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct PublishFlags(u8);

impl PublishFlags {
    /// The broker retains this message as the topic's last known value.
    const RETAIN: u8 = 0x01;
    /// A redelivery of a message already sent.
    const DUP: u8 = 0x08;
    /// Bits 1–2 hold the delivery guarantee.
    const QOS_SHIFT: u8 = 1;

    /// Read the nibble as it arrived.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The nibble to put on the wire.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The flags describing a given PUBLISH.
    const fn of(publish: &Publish) -> Self {
        let mut bits = publish.qos.bits() << Self::QOS_SHIFT;
        if publish.retain {
            bits |= Self::RETAIN;
        }
        if publish.dup {
            bits |= Self::DUP;
        }
        Self(bits)
    }

    /// Whether this is the topic's retained value.
    pub const fn retain(self) -> bool {
        self.0 & Self::RETAIN != 0
    }

    /// Whether this is a redelivery.
    pub const fn dup(self) -> bool {
        self.0 & Self::DUP != 0
    }

    /// The delivery guarantee.
    ///
    /// # Errors
    ///
    /// [`CodecError::UnsupportedQoS`] if the two bits say 2 or 3.
    pub const fn qos(self) -> Result<QoS, CodecError> {
        QoS::from_bits((self.0 >> Self::QOS_SHIFT) & 0x03)
    }
}

/// Delivery guarantee. The device uses QoS 1 for its uplink and subscribes at QoS 1.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QoS {
    /// Fire and forget.
    AtMostOnce,
    /// Acknowledged with PUBACK.
    AtLeastOnce,
}

impl QoS {
    /// Parse from the wire representation.
    ///
    /// # Errors
    ///
    /// [`CodecError::UnsupportedQoS`] for 2 or 3. QoS 2 is a three-packet handshake this device never
    /// uses, and 3 is malformed.
    pub const fn from_bits(bits: u8) -> Result<Self, CodecError> {
        match bits {
            0 => Ok(Self::AtMostOnce),
            1 => Ok(Self::AtLeastOnce),
            qos => Err(CodecError::UnsupportedQoS { qos }),
        }
    }

    /// The wire representation.
    pub const fn bits(self) -> u8 {
        match self {
            Self::AtMostOnce => 0,
            Self::AtLeastOnce => 1,
        }
    }
}

impl fmt::Display for QoS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.bits())
    }
}

/// A CONNECT packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    /// Protocol level; 4 for MQTT 3.1.1.
    pub protocol_level: u8,
    /// Client identifier. The device sends its serial.
    pub client_id: String,
    /// Username, if the flags said one is present. The device sends its serial.
    pub username: Option<String>,
    /// Password, if present. The device sends the literal `Growatt`.
    ///
    /// Not a secret and not authentication: the username is printed on the device and the password is
    /// a firmware constant shared across the product line.
    pub password: Option<Vec<u8>>,
    /// Keepalive in seconds. The device asks for 420.
    pub keepalive: u16,
    /// Whether the clean-session flag was set. The device does **not** set it.
    pub clean_session: bool,
    /// What the broker should publish if this connection dies without a DISCONNECT.
    ///
    /// The device sets none, so this is `None` on every CONNECT this program receives. It is set on the
    /// CONNECT this program *sends* to a Home Assistant broker, where it is the entire availability
    /// mechanism: a bridge that is killed cannot announce its own absence, so the broker does it.
    pub will: Option<Will>,
}

/// A last-will message, published by the broker when a connection drops uncleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Will {
    /// Topic to publish on.
    pub topic: String,
    /// Payload to publish.
    pub payload: Vec<u8>,
    /// Delivery guarantee for the will publish.
    pub qos: QoS,
    /// Whether the broker retains it, so a subscriber connecting later still sees it.
    pub retain: bool,
}

/// A PUBLISH packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    /// Topic name.
    pub topic: String,
    /// Delivery guarantee.
    pub qos: QoS,
    /// Retain flag.
    pub retain: bool,
    /// Duplicate-delivery flag.
    pub dup: bool,
    /// Packet identifier, present when QoS is above 0.
    pub packet_id: Option<u16>,
    /// The payload, which for this device is one protocol frame.
    pub payload: Vec<u8>,
}

/// A SUBSCRIBE packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribe {
    /// Packet identifier, echoed in the SUBACK.
    pub packet_id: u16,
    /// Requested topic filters and their QoS.
    pub filters: Vec<(String, u8)>,
}

/// One MQTT packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// Device to server, once per session.
    Connect(Connect),
    /// Server to device, answering CONNECT.
    ConnAck {
        /// Whether a session was resumed. Always false here: no session state is kept.
        session_present: bool,
        /// Return code; 0 is success.
        code: u8,
    },
    /// Either direction.
    Publish(Publish),
    /// Either direction, acknowledging a QoS-1 PUBLISH.
    PubAck {
        /// Identifier of the packet being acknowledged.
        packet_id: u16,
    },
    /// Device to server.
    Subscribe(Subscribe),
    /// Server to device, answering SUBSCRIBE.
    SubAck {
        /// Identifier from the SUBSCRIBE.
        packet_id: u16,
        /// Granted QoS per filter, or 0x80 for a refusal.
        granted: Vec<u8>,
    },
    /// Device to server, keepalive.
    PingReq,
    /// Server to device, answering PINGREQ.
    PingResp,
    /// Either direction, a clean close.
    Disconnect,
}

impl Packet {
    /// The packet type name, for logs.
    pub const fn kind(&self) -> &'static str {
        match *self {
            Self::Connect(_) => "CONNECT",
            Self::ConnAck { .. } => "CONNACK",
            Self::Publish(_) => "PUBLISH",
            Self::PubAck { .. } => "PUBACK",
            Self::Subscribe(_) => "SUBSCRIBE",
            Self::SubAck { .. } => "SUBACK",
            Self::PingReq => "PINGREQ",
            Self::PingResp => "PINGRESP",
            Self::Disconnect => "DISCONNECT",
        }
    }

    /// Try to decode one packet of any type from the front of `buf`.
    ///
    /// Returns `Ok(None)` when more octets are needed, which is the normal case on a stream. The
    /// `usize` is how many octets the packet consumed.
    ///
    /// Decodes both directions. Use [`Packet::decode_from_device`] on the server's input, where a
    /// server-to-device packet is a protocol error rather than something to interpret. This general
    /// form exists because the cloud relay will read the other direction, and because a test that
    /// checks what the server replied has to parse those replies.
    ///
    /// # Errors
    ///
    /// [`CodecError`] if the octets present are malformed or describe a packet type outside MQTT 3.1.1.
    /// A decode error is fatal for a connection: once framing is lost there is no resynchronising.
    pub fn decode(buf: &[u8]) -> Result<Option<(Self, usize)>, CodecError> {
        let mut header = Reader::new(buf);
        let Some(first) = header.u8() else {
            return Ok(None);
        };
        let kind = first >> 4;
        let flags = first & 0x0F;

        let Some(remaining) = header.varint()? else {
            return Ok(None);
        };

        let header_len = header.position();
        let total = header_len.saturating_add(remaining);
        if buf.len() < total {
            return Ok(None);
        }
        let body = buf.get(header_len..total).unwrap_or_default();

        let packet_id_of = |kind: &'static str| {
            Reader::new(body).u16().ok_or(CodecError::Truncated {
                kind,
                field: "packet identifier",
            })
        };

        let packet = match kind {
            1 => Self::Connect(Connect::decode(body)?),
            2 => Self::ConnAck {
                session_present: body.first().copied().unwrap_or(0) & 0x01 != 0,
                code: body.get(1).copied().ok_or(CodecError::Truncated {
                    kind: "CONNACK",
                    field: "return code",
                })?,
            },
            3 => Self::Publish(Publish::decode(body, flags)?),
            4 => Self::PubAck {
                packet_id: packet_id_of("PUBACK")?,
            },
            8 => Self::Subscribe(Subscribe::decode(body)?),
            9 => Self::SubAck {
                packet_id: packet_id_of("SUBACK")?,
                granted: body.get(2..).unwrap_or_default().to_vec(),
            },
            12 => Self::PingReq,
            13 => Self::PingResp,
            14 => Self::Disconnect,
            // 5, 6, 7 are the QoS-2 handshake; 10, 11 are UNSUBSCRIBE and UNSUBACK. None appear in this
            // device's traffic, and implementing them would be implementing behaviour never observed.
            kind => return Err(CodecError::UnsupportedType { kind }),
        };

        Ok(Some((packet, total)))
    }

    /// Decode a packet arriving at the server from the device.
    ///
    /// Rejects the three server-to-device types. A device that sends CONNACK, SUBACK or PINGRESP is
    /// either broken or not the device, and either way the session should end rather than the packet be
    /// interpreted.
    ///
    /// # Errors
    ///
    /// As [`Packet::decode`], plus [`CodecError::UnsupportedType`] for a server-to-device packet.
    pub fn decode_from_device(buf: &[u8]) -> Result<Option<(Self, usize)>, CodecError> {
        let Some((packet, used)) = Self::decode(buf)? else {
            return Ok(None);
        };
        match packet {
            Self::ConnAck { .. } => Err(CodecError::UnsupportedType { kind: 2 }),
            Self::SubAck { .. } => Err(CodecError::UnsupportedType { kind: 9 }),
            Self::PingResp => Err(CodecError::UnsupportedType { kind: 13 }),
            packet => Ok(Some((packet, used))),
        }
    }

    /// Serialise for transmission.
    ///
    /// # Errors
    ///
    /// [`CodecError::TooLong`] if the payload exceeds what a 4-octet varint can describe.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        let (first_octet, body) = match self {
            Self::ConnAck { session_present, code } => (0x20, vec![u8::from(*session_present), *code]),

            Self::PubAck { packet_id } => (0x40, packet_id.to_be_bytes().to_vec()),

            Self::SubAck { packet_id, granted } => {
                let mut body = packet_id.to_be_bytes().to_vec();
                body.extend_from_slice(granted);
                (0x90, body)
            }

            Self::PingResp => (0xD0, Vec::new()),

            Self::Publish(publish) => (0x30 | publish.flags(), publish.encode_body()),

            Self::Disconnect => (0xE0, Vec::new()),
            Self::PingReq => (0xC0, Vec::new()),

            // Device-to-server packets. Encoded only by the cloud relay, which connects upstream *as*
            // the device — that is the whole point of a relay, and the cloud is a third party that may
            // care about the details, so they are reproduced exactly rather than approximated.
            Self::Connect(connect) => (0x10, connect.encode_body()),

            // Bit 1 of the flags nibble is mandatory for SUBSCRIBE.
            Self::Subscribe(subscribe) => (0x82, subscribe.encode_body()),
        };

        if body.len() > MAX_REMAINING_LEN {
            return Err(CodecError::TooLong { len: body.len() });
        }

        let mut out = Writer::new();
        out.u8(first_octet);
        out.varint(body.len());
        out.raw(&body);
        Ok(out.finish())
    }
}

impl Connect {
    /// Decode from a CONNECT body, i.e. the octets after the fixed header.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] or [`CodecError::NotUtf8`], naming the field that failed.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let name = reader.string().ok_or(CodecError::Truncated {
            kind: "CONNECT",
            field: "protocol name",
        })?;
        let name = core::str::from_utf8(name).map_err(|_| CodecError::NotUtf8 { field: "protocol name" })?;
        if name != PROTOCOL_NAME {
            // Not fatal to decode, but the session layer refuses it. Recorded rather than rejected here
            // so the log can say what was actually offered.
            tracing::warn!(protocol_name = name, "CONNECT carried an unexpected protocol name");
        }

        let protocol_level = reader.u8().ok_or(CodecError::Truncated {
            kind: "CONNECT",
            field: "protocol level",
        })?;
        let flags = ConnectFlags::from_bits(reader.u8().ok_or(CodecError::Truncated {
            kind: "CONNECT",
            field: "connect flags",
        })?);
        let keepalive = reader.u16().ok_or(CodecError::Truncated {
            kind: "CONNECT",
            field: "keepalive",
        })?;

        let client_id = reader.utf8_string("client identifier")?;

        // The device sets no will, so this is `None` on every CONNECT received here. Decoded rather than
        // skipped so a round trip stays exact for anything that does set one, and because this program
        // sends a will of its own to the Home Assistant broker.
        let will = if flags.has_will() {
            Some(Will {
                topic: reader.utf8_string("will topic")?,
                payload: reader
                    .string()
                    .ok_or(CodecError::Truncated {
                        kind: "CONNECT",
                        field: "will message",
                    })?
                    .to_vec(),
                qos: flags.will_qos()?,
                retain: flags.will_retain(),
            })
        } else {
            None
        };

        let username = if flags.has_username() {
            Some(reader.utf8_string("username")?)
        } else {
            None
        };

        let password = if flags.has_password() {
            Some(
                reader
                    .string()
                    .ok_or(CodecError::Truncated {
                        kind: "CONNECT",
                        field: "password",
                    })?
                    .to_vec(),
            )
        } else {
            None
        };

        Ok(Self {
            protocol_level,
            client_id,
            username,
            password,
            keepalive,
            clean_session: flags.clean_session(),
            will,
        })
    }

    /// Encode the body, without the fixed header.
    fn encode_body(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(PROTOCOL_NAME);
        writer.u8(self.protocol_level);
        writer.u8(ConnectFlags::of(self).bits());
        writer.u16(self.keepalive);
        writer.string(&self.client_id);
        // Order is fixed by the protocol: will fields, then username, then password.
        if let Some(will) = self.will.as_ref() {
            writer.string(&will.topic);
            writer.bytes(&will.payload);
        }
        if let Some(username) = self.username.as_deref() {
            writer.string(username);
        }
        if let Some(password) = self.password.as_deref() {
            writer.bytes(password);
        }
        writer.finish()
    }
}

impl Publish {
    /// Decode from a PUBLISH body and the flags nibble of its fixed header.
    ///
    /// # Errors
    ///
    /// [`CodecError::UnsupportedQoS`] for QoS 2, or [`CodecError::Truncated`] / [`CodecError::NotUtf8`].
    pub fn decode(body: &[u8], flags: u8) -> Result<Self, CodecError> {
        let flags = PublishFlags::from_bits(flags);
        let qos = flags.qos()?;
        let mut reader = Reader::new(body);
        let topic = reader.utf8_string("topic")?;

        let packet_id = if qos == QoS::AtMostOnce {
            None
        } else {
            Some(reader.u16().ok_or(CodecError::Truncated {
                kind: "PUBLISH",
                field: "packet identifier",
            })?)
        };

        Ok(Self {
            topic,
            qos,
            retain: flags.retain(),
            dup: flags.dup(),
            packet_id,
            payload: reader.rest().to_vec(),
        })
    }

    /// The flags nibble this publish needs in its fixed header.
    const fn flags(&self) -> u8 {
        PublishFlags::of(self).bits()
    }

    /// Encode the body, without the fixed header.
    fn encode_body(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.topic);
        if self.qos != QoS::AtMostOnce {
            writer.u16(self.packet_id.unwrap_or(1));
        }
        writer.raw(&self.payload);
        writer.finish()
    }
}

impl Subscribe {
    /// Decode from a SUBSCRIBE body.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] or [`CodecError::NotUtf8`], naming the field that failed.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let packet_id = reader.u16().ok_or(CodecError::Truncated {
            kind: "SUBSCRIBE",
            field: "packet identifier",
        })?;

        let mut filters = Vec::new();
        while reader.remaining() > 0 {
            let filter = reader.utf8_string("topic filter")?;
            let qos = reader.u8().ok_or(CodecError::Truncated {
                kind: "SUBSCRIBE",
                field: "requested QoS",
            })?;
            filters.push((filter, qos));
        }

        Ok(Self { packet_id, filters })
    }

    /// Encode the body, without the fixed header.
    fn encode_body(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u16(self.packet_id);
        for (filter, qos) in &self.filters {
            writer.string(filter);
            writer.u8(*qos);
        }
        writer.finish()
    }
}

/// A cursor over a packet body that never indexes out of range.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let value = self.buf.get(self.pos).copied()?;
        self.pos = self.pos.saturating_add(1);
        Some(value)
    }

    fn u16(&mut self) -> Option<u16> {
        let end = self.pos.checked_add(2)?;
        let value = match *self.buf.get(self.pos..end)? {
            [hi, lo] => u16::from_be_bytes([hi, lo]),
            _ => return None,
        };
        self.pos = end;
        Some(value)
    }

    /// A length-prefixed byte string.
    fn string(&mut self) -> Option<&'a [u8]> {
        let len = usize::from(self.u16()?);
        let end = self.pos.checked_add(len)?;
        let value = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(value)
    }

    /// A length-prefixed string, required to be UTF-8.
    fn utf8_string(&mut self, field: &'static str) -> Result<String, CodecError> {
        let raw = self.string().ok_or(CodecError::Truncated { kind: "packet", field })?;
        core::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| CodecError::NotUtf8 { field })
    }

    fn rest(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or_default()
    }

    const fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// A remaining-length varint.
    ///
    /// `Ok(None)` means the octets present are a valid prefix but the varint is incomplete — the normal
    /// case at the head of a stream.
    fn varint(&mut self) -> Result<Option<usize>, CodecError> {
        let mut value = 0usize;
        let mut multiplier = 1usize;

        for _ in 0..4u8 {
            let Some(octet) = self.u8() else {
                return Ok(None);
            };
            value = value.saturating_add(usize::from(octet & 0x7F).saturating_mul(multiplier));
            if octet & 0x80 == 0 {
                return Ok(Some(value));
            }
            multiplier = multiplier.saturating_mul(128);
        }

        Err(CodecError::MalformedLength)
    }

    /// How many octets have been consumed.
    const fn position(&self) -> usize {
        self.pos
    }
}

/// The counterpart to [`Reader`]: accumulates the octets of a packet.
///
/// Exists so that encoding reads as the mirror of decoding, and so the length-prefix rule lives in one
/// place instead of at each call site.
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// A length-prefixed UTF-8 string.
    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// A length-prefixed byte string.
    ///
    /// The CONNECT password is a binary field in MQTT 3.1.1, even though this device sends ASCII in it.
    fn bytes(&mut self, value: &[u8]) {
        self.u16(u16::try_from(value.len()).unwrap_or(u16::MAX));
        self.raw(value);
    }

    /// Octets with no length prefix, e.g. a publish payload.
    fn raw(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    /// A remaining-length varint.
    fn varint(&mut self, mut value: usize) {
        loop {
            let mut octet = u8::try_from(value % 128).unwrap_or(0);
            value /= 128;
            if value > 0 {
                octet |= 0x80;
            }
            self.buf.push(octet);
            if value == 0 {
                return;
            }
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// A stream framed into MQTT packets.
///
/// Owns the read buffer, which is what turns a pair of free functions taking `(&mut stream, &mut buf)`
/// into two methods. Both directions of the program use it: the device-facing session and the cloud
/// relay had the same loop before this existed.
///
/// Generic over the transport, so the whole thing can be driven over an in-memory duplex with no TLS
/// and no sockets.
#[derive(Debug)]
pub struct PacketStream<S> {
    stream: S,
    buf: Vec<u8>,
    limit: usize,
}

/// Why a framed read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The transport failed.
    #[error("stream i/o failed: {0}")]
    Io(#[from] std::io::Error),

    /// The octets are not a valid packet. Fatal: framing cannot be resynchronised on a stream.
    #[error("{0}")]
    Codec(#[from] CodecError),

    /// The peer announced more octets than this stream will buffer.
    #[error("peer announced a {len}-octet packet, above the {limit}-octet limit")]
    TooLarge {
        /// Octets buffered so far.
        len: usize,
        /// The configured limit.
        limit: usize,
    },
}

impl<S> PacketStream<S> {
    /// Wrap a transport, buffering at most `limit` octets for one packet.
    pub fn new(stream: S, limit: usize) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(1024),
            limit,
        }
    }

    /// The wrapped transport.
    pub const fn get_ref(&self) -> &S {
        &self.stream
    }
}

impl<S> PacketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    /// Read the next whole packet, or `None` when the peer closes.
    ///
    /// Cancel-safe: partial octets stay buffered, so this can sit in a `select!` arm that loses.
    ///
    /// # Errors
    ///
    /// [`StreamError`] on transport failure, a malformed packet, or one above the size limit.
    pub async fn next_packet(&mut self) -> Result<Option<Packet>, StreamError> {
        use tokio::io::AsyncReadExt as _;

        loop {
            if let Some((packet, used)) = Packet::decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(Some(packet));
            }

            if self.buf.len() > self.limit {
                return Err(StreamError::TooLarge {
                    len: self.buf.len(),
                    limit: self.limit,
                });
            }

            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(chunk.get(..read).unwrap_or_default());
        }
    }

    /// Read the next packet, refusing the three server-to-device types.
    ///
    /// For the device-facing side, where a device sending CONNACK is either broken or not the device.
    ///
    /// # Errors
    ///
    /// As [`PacketStream::next_packet`], plus a codec error for a server-to-device packet.
    pub async fn next_packet_from_device(&mut self) -> Result<Option<Packet>, StreamError> {
        use tokio::io::AsyncReadExt as _;

        loop {
            if let Some((packet, used)) = Packet::decode_from_device(&self.buf)? {
                self.buf.drain(..used);
                return Ok(Some(packet));
            }

            if self.buf.len() > self.limit {
                return Err(StreamError::TooLarge {
                    len: self.buf.len(),
                    limit: self.limit,
                });
            }

            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(chunk.get(..read).unwrap_or_default());
        }
    }

    /// Write one packet and flush it.
    ///
    /// # Errors
    ///
    /// [`StreamError`] if the packet cannot be encoded or the transport fails.
    pub async fn send(&mut self, packet: &Packet) -> Result<(), StreamError> {
        use tokio::io::AsyncWriteExt as _;

        let wire = packet.encode()?;
        tracing::trace!(kind = packet.kind(), len = wire.len(), "sending");
        self.stream.write_all(&wire).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodecError, Connect, ConnectFlags, PROTOCOL_LEVEL, Packet, PacketStream, Publish, QoS, Reader, Subscribe, Will,
        Writer,
    };

    const SERIAL: &str = "0EXAMPLE00000001";

    /// A string's length as the 16-bit prefix MQTT uses.
    ///
    /// Every string in these tests is a short literal, so the conversion cannot fail — but saying so
    /// with `try_from` rather than `as` keeps the truncating-cast lint meaningful everywhere else.
    fn len16(value: &str) -> u16 {
        u16::try_from(value.len()).expect("test strings are short")
    }

    /// Wrap a hand-built body in a fixed header.
    ///
    /// These tests build bodies by hand on purpose: they are the reference octets the encoder is checked
    /// against, so going through the encoder to produce them would be checking it against itself.
    fn framed(first_octet: u8, body: &[u8]) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u8(first_octet);
        writer.varint(body.len());
        writer.raw(body);
        writer.finish()
    }

    /// A CONNECT exactly as the device sends it: flags 0xC0, keepalive 420, password `Growatt`.
    fn device_connect() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x00, 0x04]);
        body.extend_from_slice(b"MQTT");
        body.push(0x04); // level
        body.push(0xC0); // username + password, clean session NOT set
        body.extend_from_slice(&420u16.to_be_bytes());
        body.extend_from_slice(&len16(SERIAL).to_be_bytes());
        body.extend_from_slice(SERIAL.as_bytes());
        body.extend_from_slice(&len16(SERIAL).to_be_bytes());
        body.extend_from_slice(SERIAL.as_bytes());
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"Growatt");
        framed(0x10, &body)
    }

    #[test]
    fn decodes_the_device_connect() {
        let wire = device_connect();
        let (packet, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        match packet {
            Packet::Connect(Connect {
                protocol_level,
                client_id,
                username,
                password,
                keepalive,
                clean_session,
                will,
            }) => {
                assert_eq!(protocol_level, 4);
                assert_eq!(client_id, SERIAL);
                assert_eq!(will, None, "the device sets no last will");
                assert_eq!(username.as_deref(), Some(SERIAL));
                assert_eq!(password.as_deref(), Some(b"Growatt".as_slice()));
                assert_eq!(keepalive, 420);
                assert!(!clean_session, "the device does not set clean session");
            }
            other => panic!("expected CONNECT, got {}", other.kind()),
        }
    }

    #[test]
    fn a_partial_packet_asks_for_more_rather_than_failing() {
        let wire = device_connect();
        for cut in 1..wire.len() {
            let partial = wire.get(..cut).expect("prefix");
            assert_eq!(
                Packet::decode(partial).expect("no error on a prefix"),
                None,
                "a {cut}-octet prefix should ask for more"
            );
        }
        assert_eq!(Packet::decode(&[]).expect("empty"), None);
    }

    #[test]
    fn decodes_a_qos1_publish_with_its_packet_id() {
        let payload = [0xAA, 0xBB, 0xCC];
        let topic = format!("c/33/{SERIAL}");
        let mut body = Vec::new();
        body.extend_from_slice(&len16(&topic).to_be_bytes());
        body.extend_from_slice(topic.as_bytes());
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(&payload);
        let wire = framed(0x32, &body); // PUBLISH, QoS 1

        let (packet, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        match packet {
            Packet::Publish(Publish {
                topic: got,
                qos,
                packet_id,
                payload: got_payload,
                retain,
                dup,
            }) => {
                assert_eq!(got, topic);
                assert_eq!(qos, QoS::AtLeastOnce);
                assert_eq!(packet_id, Some(7));
                assert_eq!(got_payload, payload);
                assert!(!retain);
                assert!(!dup);
            }
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }
    }

    #[test]
    fn decodes_the_device_subscribe() {
        let filter = format!("s/33/{SERIAL}");
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&len16(&filter).to_be_bytes());
        body.extend_from_slice(filter.as_bytes());
        body.push(0x01); // requested QoS 1
        let wire = framed(0x82, &body);

        let (packet, _) = Packet::decode(&wire).expect("decode").expect("complete");
        match packet {
            Packet::Subscribe(Subscribe { packet_id, filters }) => {
                assert_eq!(packet_id, 1);
                assert_eq!(filters, vec![(filter, 1)]);
            }
            other => panic!("expected SUBSCRIBE, got {}", other.kind()),
        }
    }

    #[test]
    fn encodes_connack_suback_puback_pingresp() {
        assert_eq!(
            Packet::ConnAck {
                session_present: false,
                code: 0
            }
            .encode()
            .expect("encode"),
            vec![0x20, 0x02, 0x00, 0x00]
        );
        assert_eq!(
            Packet::SubAck {
                packet_id: 1,
                granted: vec![0x01]
            }
            .encode()
            .expect("encode"),
            vec![0x90, 0x03, 0x00, 0x01, 0x01]
        );
        assert_eq!(
            Packet::PubAck { packet_id: 7 }.encode().expect("encode"),
            vec![0x40, 0x02, 0x00, 0x07]
        );
        assert_eq!(Packet::PingResp.encode().expect("encode"), vec![0xD0, 0x00]);
    }

    #[test]
    fn suback_grants_qos_one() {
        // The single most important octet this server emits. Granting 0 produces a device that
        // connects, subscribes, looks healthy and publishes nothing at all.
        let wire = Packet::SubAck {
            packet_id: 1,
            granted: vec![0x01],
        }
        .encode()
        .expect("encode");
        assert_eq!(wire.last(), Some(&0x01), "SUBACK must grant QoS 1");
    }

    #[test]
    fn publish_round_trips() {
        let publish = Publish {
            topic: format!("s/{SERIAL}"),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(3),
            payload: vec![1, 2, 3, 4],
        };
        let wire = Packet::Publish(publish.clone()).encode().expect("encode");
        let (decoded, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        assert_eq!(decoded, Packet::Publish(publish));
    }

    #[test]
    fn qos_zero_publish_has_no_packet_id() {
        let publish = Publish {
            topic: "s/x".to_owned(),
            qos: QoS::AtMostOnce,
            retain: false,
            dup: false,
            packet_id: None,
            payload: vec![9],
        };
        let wire = Packet::Publish(publish.clone()).encode().expect("encode");
        let (decoded, _) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(decoded, Packet::Publish(publish));
    }

    #[test]
    fn pingreq_and_disconnect_are_two_octets() {
        let (packet, used) = Packet::decode(&[0xC0, 0x00]).expect("d").expect("complete");
        assert_eq!(packet, Packet::PingReq);
        assert_eq!(used, 2);
        let (packet, used) = Packet::decode(&[0xE0, 0x00]).expect("d").expect("complete");
        assert_eq!(packet, Packet::Disconnect);
        assert_eq!(used, 2);
    }

    #[test]
    fn server_to_device_types_are_refused_on_input() {
        // A device sending CONNACK is a protocol error, not something to tolerate.
        let connack = Packet::ConnAck {
            session_present: false,
            code: 0,
        }
        .encode()
        .expect("encode");
        let suback = Packet::SubAck {
            packet_id: 1,
            granted: vec![1],
        }
        .encode()
        .expect("encode");

        for wire in [connack, suback, vec![0xD0, 0x00]] {
            assert!(
                matches!(
                    Packet::decode_from_device(&wire),
                    Err(CodecError::UnsupportedType { .. })
                ),
                "{wire:02x?} should be refused as device input"
            );
            // ...but the general decoder handles it, which is what the relay and the tests need.
            assert!(Packet::decode(&wire).expect("general decode").is_some());
        }
    }

    #[test]
    fn the_device_connect_survives_a_re_encode_byte_for_byte() {
        // What the relay depends on: decode the device's CONNECT, encode it again for the cloud, and get
        // the same octets. Anything less means the cloud sees a subtly different client than the device.
        let original = device_connect();
        let (packet, used) = Packet::decode(&original).expect("decode").expect("complete");
        assert_eq!(used, original.len());
        assert_eq!(packet.encode().expect("re-encode"), original);
    }

    #[test]
    fn a_re_encoded_connect_keeps_the_flags_the_device_set() {
        let (packet, _) = Packet::decode(&device_connect()).expect("d").expect("complete");
        let wire = packet.encode().expect("encode");
        // Flags octet sits after the protocol name and level: 2 + 4 + 1.
        let flags = wire.get(2 + 2 + 4 + 1).copied().expect("flags octet");
        assert_eq!(flags, 0xC0, "username and password set, clean session not");
        assert_eq!(flags & 0x02, 0, "clean session must stay unset");
    }

    #[test]
    fn subscribe_round_trips() {
        let original = Subscribe {
            packet_id: 1,
            filters: vec![(format!("s/33/{SERIAL}"), 1), (format!("s/{SERIAL}"), 1)],
        };
        let wire = Packet::Subscribe(original.clone()).encode().expect("encode");
        assert_eq!(wire.first(), Some(&0x82), "SUBSCRIBE needs flags 0x02");
        let (decoded, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        assert_eq!(decoded, Packet::Subscribe(original));
    }

    #[test]
    fn a_connect_carrying_a_will_round_trips() {
        // The will is what a broker publishes when this program dies without saying goodbye, so it is
        // the availability mechanism for Home Assistant. Nothing else in this codebase sends one, which
        // is exactly why it needs a test of its own.
        let original = Connect {
            protocol_level: PROTOCOL_LEVEL,
            client_id: "heliobridge".to_owned(),
            username: Some("ha".to_owned()),
            password: Some(b"secret".to_vec()),
            keepalive: 60,
            clean_session: true,
            will: Some(Will {
                topic: "heliobridge/0EXAMPLE00000001/availability".to_owned(),
                payload: b"offline".to_vec(),
                qos: QoS::AtLeastOnce,
                retain: true,
            }),
        };
        let wire = Packet::Connect(original.clone()).encode().expect("encode");
        let (decoded, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        assert_eq!(decoded, Packet::Connect(original));
    }

    #[test]
    fn the_will_flags_describe_the_will() {
        let flags = ConnectFlags::of(&Connect {
            protocol_level: PROTOCOL_LEVEL,
            client_id: "heliobridge".to_owned(),
            username: None,
            password: None,
            keepalive: 60,
            clean_session: true,
            will: Some(Will {
                topic: "t".to_owned(),
                payload: b"offline".to_vec(),
                qos: QoS::AtLeastOnce,
                retain: true,
            }),
        });

        assert!(flags.has_will());
        assert!(flags.will_retain());
        assert!(flags.clean_session());
        assert!(!flags.has_username());
        assert!(!flags.has_password());
        assert_eq!(flags.will_qos(), Ok(QoS::AtLeastOnce));
        // Will + will-QoS-1 + will-retain + clean-session, and nothing else.
        assert_eq!(flags.bits(), 0x04 | 0x08 | 0x20 | 0x02);
    }

    #[test]
    fn a_will_that_is_absent_leaves_every_will_bit_clear() {
        // The guard on the device's own CONNECT: the relay re-encodes it upstream, so a stray will bit
        // would be a difference the vendor cloud could see.
        let (packet, _) = Packet::decode(&device_connect()).expect("decode").expect("complete");
        let Packet::Connect(connect) = packet else {
            panic!("not a CONNECT");
        };
        let flags = ConnectFlags::of(&connect);
        assert!(!flags.has_will());
        assert!(!flags.will_retain());
        assert_eq!(flags.will_qos(), Ok(QoS::AtMostOnce));
    }

    #[test]
    fn server_replies_round_trip_through_the_general_decoder() {
        for original in [
            Packet::ConnAck {
                session_present: false,
                code: 0,
            },
            Packet::SubAck {
                packet_id: 42,
                granted: vec![0x01],
            },
            Packet::PingResp,
        ] {
            let wire = original.encode().expect("encode");
            let (decoded, used) = Packet::decode(&wire).expect("decode").expect("complete");
            assert_eq!(used, wire.len());
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn qos2_and_unsubscribe_are_refused() {
        // Never seen from this device; implementing them would be implementing unobserved behaviour.
        for kind in [5u8, 6, 7, 10, 11] {
            let wire = [kind << 4, 0x02, 0x00, 0x01];
            assert!(
                matches!(Packet::decode(&wire), Err(CodecError::UnsupportedType { .. })),
                "packet type {kind} should be refused"
            );
        }
    }

    #[test]
    fn qos_two_is_refused() {
        // 0x34 is PUBLISH with QoS 2.
        let wire = framed(0x34, &[0x00, 0x01, b'x', 0x00, 0x01]);
        assert!(matches!(
            Packet::decode(&wire),
            Err(CodecError::UnsupportedQoS { qos: 2 })
        ));
    }

    #[test]
    fn varints_round_trip_across_their_boundaries() {
        for value in [0usize, 1, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152] {
            let mut writer = Writer::new();
            writer.varint(value);
            let out = writer.finish();

            let mut reader = Reader::new(&out);
            let decoded = reader.varint().expect("decode").expect("complete");
            assert_eq!(decoded, value, "value {value}");
            assert_eq!(reader.position(), out.len(), "value {value}");
        }
    }

    #[test]
    fn an_incomplete_varint_asks_for_more() {
        // A continuation bit with nothing after it is a prefix, not a malformed packet.
        let mut reader = Reader::new(&[0x80]);
        assert_eq!(reader.varint().expect("no error"), None);
    }

    #[test]
    fn an_overlong_varint_is_rejected() {
        let mut reader = Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert_eq!(reader.varint(), Err(CodecError::MalformedLength));
    }

    #[test]
    fn a_585_octet_frame_needs_a_two_octet_varint() {
        // Telemetry is 585 octets plus the topic, so the length crosses the 127 boundary. This is the
        // case a single-octet length assumption breaks on.
        let topic = format!("c/33/{SERIAL}");
        let mut body = Vec::new();
        body.extend_from_slice(&len16(&topic).to_be_bytes());
        body.extend_from_slice(topic.as_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&[0x5A; 585]);
        let wire = framed(0x32, &body);

        assert!(body.len() > 127, "the length must need two octets");
        let (packet, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        match packet {
            Packet::Publish(p) => assert_eq!(p.payload.len(), 585),
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }
    }

    #[tokio::test]
    async fn a_packet_stream_frames_across_read_boundaries() {
        use tokio::io::AsyncWriteExt as _;

        // The case that breaks a naive reader: several packets arriving in one read, and one packet
        // split across two.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut wire = Packet::PingReq.encode().expect("encode");
        wire.extend_from_slice(&Packet::PubAck { packet_id: 5 }.encode().expect("encode"));
        let big = Packet::Publish(Publish {
            topic: format!("c/33/{SERIAL}"),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(9),
            payload: vec![0x5A; 585],
        })
        .encode()
        .expect("encode");

        let split = big.len().wrapping_div(2);
        client.write_all(&wire).await.expect("write");
        client
            .write_all(big.get(..split).expect("head"))
            .await
            .expect("write head");

        let mut stream = PacketStream::new(server, 64 * 1024);
        assert_eq!(stream.next_packet().await.expect("read"), Some(Packet::PingReq));
        assert_eq!(
            stream.next_packet().await.expect("read"),
            Some(Packet::PubAck { packet_id: 5 })
        );

        // The third packet is incomplete; finish it and the same call resolves.
        client
            .write_all(big.get(split..).expect("tail"))
            .await
            .expect("write tail");
        match stream.next_packet().await.expect("read") {
            Some(Packet::Publish(publish)) => assert_eq!(publish.payload.len(), 585),
            other => panic!("expected PUBLISH, got {other:?}"),
        }

        // Closing the far end reports end of stream rather than an error.
        drop(client);
        assert_eq!(stream.next_packet().await.expect("read"), None);
    }

    #[tokio::test]
    async fn a_packet_stream_refuses_server_packets_from_a_device() {
        use tokio::io::AsyncWriteExt as _;

        let (mut client, server) = tokio::io::duplex(4096);
        let connack = Packet::ConnAck {
            session_present: false,
            code: 0,
        }
        .encode()
        .expect("encode");
        client.write_all(&connack).await.expect("write");

        let mut stream = PacketStream::new(server, 4096);
        assert!(stream.next_packet_from_device().await.is_err());
    }

    #[test]
    fn two_packets_in_one_buffer_decode_one_at_a_time() {
        // A stream can deliver several packets in one read; the caller must be able to advance.
        let mut buf2 = vec![0xC0, 0x00];
        buf2.extend_from_slice(&Packet::PubAck { packet_id: 5 }.encode().expect("encode"));

        let (first, used) = Packet::decode(&buf2).expect("d").expect("complete");
        assert_eq!(first, Packet::PingReq);
        let rest = buf2.get(used..).expect("remainder");
        let (second, used2) = Packet::decode(rest).expect("d").expect("complete");
        assert_eq!(second, Packet::PubAck { packet_id: 5 });
        assert_eq!(used.saturating_add(used2), buf2.len());
    }
}
