//! MQTT 3.1.1 packet codec — the subset one device uses.
//!
//! Nine packet types, a fixed header, a varint length and length-prefixed strings. This is
//! deliberately hand-written rather than taken from a crate: the surface is small and fully
//! specified, every candidate crate is either unmaintained or a fresh personal fork, and this sits on
//! the most critical path in the program. When the device does something unexpected, the parser is
//! readable.
//!
//! What is **not** implemented, because the device never uses it: QoS 2, retained messages, wildcard
//! subscriptions, topic aliases, will messages, and session state across reconnects.

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
        let Some(first) = buf.first().copied() else {
            return Ok(None);
        };
        let kind = first >> 4;
        let flags = first & 0x0F;

        let Some((remaining, varint_len)) = decode_varint(buf.get(1..).unwrap_or_default())? else {
            return Ok(None);
        };

        let header_len = 1usize.saturating_add(varint_len);
        let total = header_len.saturating_add(remaining);
        if buf.len() < total {
            return Ok(None);
        }
        let body = buf.get(header_len..total).unwrap_or_default();

        let packet_id_of = |kind: &'static str| {
            read_u16(body, 0).ok_or(CodecError::Truncated {
                kind,
                field: "packet identifier",
            })
        };

        let packet = match kind {
            1 => Self::Connect(decode_connect(body)?),
            2 => Self::ConnAck {
                session_present: body.first().copied().unwrap_or(0) & 0x01 != 0,
                code: body.get(1).copied().ok_or(CodecError::Truncated {
                    kind: "CONNACK",
                    field: "return code",
                })?,
            },
            3 => Self::Publish(decode_publish(body, flags)?),
            4 => Self::PubAck {
                packet_id: packet_id_of("PUBACK")?,
            },
            8 => Self::Subscribe(decode_subscribe(body)?),
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

            Self::Publish(publish) => {
                let mut first = 0x30 | (publish.qos.bits() << 1);
                if publish.retain {
                    first |= 0x01;
                }
                if publish.dup {
                    first |= 0x08;
                }
                let mut body = Vec::new();
                write_string(&mut body, &publish.topic);
                if publish.qos != QoS::AtMostOnce {
                    body.extend_from_slice(&publish.packet_id.unwrap_or(1).to_be_bytes());
                }
                body.extend_from_slice(&publish.payload);
                (first, body)
            }

            Self::Disconnect => (0xE0, Vec::new()),
            Self::PingReq => (0xC0, Vec::new()),

            // Device-to-server packets. Encoding one would mean this program is impersonating the
            // device, which it never does.
            Self::Connect(_) | Self::Subscribe(_) => {
                return Err(CodecError::UnsupportedType { kind: 0 });
            }
        };

        if body.len() > MAX_REMAINING_LEN {
            return Err(CodecError::TooLong { len: body.len() });
        }

        let mut out = Vec::with_capacity(body.len().saturating_add(5));
        out.push(first_octet);
        write_varint(&mut out, body.len());
        out.extend_from_slice(&body);
        Ok(out)
    }
}

// --- decoding helpers -----------------------------------------------------------------------------

fn decode_connect(body: &[u8]) -> Result<Connect, CodecError> {
    let mut reader = Reader::new(body);
    let name = reader.string().ok_or(CodecError::Truncated {
        kind: "CONNECT",
        field: "protocol name",
    })?;
    let name = core::str::from_utf8(name).map_err(|_| CodecError::NotUtf8 { field: "protocol name" })?;
    if name != PROTOCOL_NAME {
        // Not fatal to decode, but the session layer refuses it. Recorded rather than rejected here so
        // the log can say what was actually offered.
        tracing::warn!(protocol_name = name, "CONNECT carried an unexpected protocol name");
    }

    let protocol_level = reader.u8().ok_or(CodecError::Truncated {
        kind: "CONNECT",
        field: "protocol level",
    })?;
    let flags = reader.u8().ok_or(CodecError::Truncated {
        kind: "CONNECT",
        field: "connect flags",
    })?;
    let keepalive = reader.u16().ok_or(CodecError::Truncated {
        kind: "CONNECT",
        field: "keepalive",
    })?;

    let client_id = reader.utf8_string("client identifier")?;

    let has_will = flags & 0x04 != 0;
    if has_will {
        // The device sets no will. Skip the fields rather than fail, so a future firmware that does
        // set one still connects.
        let _topic = reader.string();
        let _message = reader.string();
    }

    let username = if flags & 0x80 == 0 {
        None
    } else {
        Some(reader.utf8_string("username")?)
    };

    let password = if flags & 0x40 == 0 {
        None
    } else {
        Some(
            reader
                .string()
                .ok_or(CodecError::Truncated {
                    kind: "CONNECT",
                    field: "password",
                })?
                .to_vec(),
        )
    };

    Ok(Connect {
        protocol_level,
        client_id,
        username,
        password,
        keepalive,
        clean_session: flags & 0x02 != 0,
    })
}

fn decode_publish(body: &[u8], flags: u8) -> Result<Publish, CodecError> {
    let qos = QoS::from_bits((flags >> 1) & 0x03)?;
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

    Ok(Publish {
        topic,
        qos,
        retain: flags & 0x01 != 0,
        dup: flags & 0x08 != 0,
        packet_id,
        payload: reader.rest().to_vec(),
    })
}

fn decode_subscribe(body: &[u8]) -> Result<Subscribe, CodecError> {
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

    Ok(Subscribe { packet_id, filters })
}

/// Decode a remaining-length varint, returning the value and how many octets it used.
fn decode_varint(buf: &[u8]) -> Result<Option<(usize, usize)>, CodecError> {
    let mut value = 0usize;
    let mut multiplier = 1usize;

    for index in 0..4usize {
        let Some(octet) = buf.get(index).copied() else {
            return Ok(None);
        };
        value = value.saturating_add(usize::from(octet & 0x7F).saturating_mul(multiplier));
        if octet & 0x80 == 0 {
            return Ok(Some((value, index.saturating_add(1))));
        }
        multiplier = multiplier.saturating_mul(128);
    }

    Err(CodecError::MalformedLength)
}

/// Append a remaining-length varint.
///
/// Visible to the crate so the session tests can build device packets by hand.
pub(crate) fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut octet = u8::try_from(value % 128).unwrap_or(0);
        value /= 128;
        if value > 0 {
            octet |= 0x80;
        }
        out.push(octet);
        if value == 0 {
            break;
        }
    }
}

/// Append a length-prefixed UTF-8 string.
fn write_string(out: &mut Vec<u8>, value: &str) {
    let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// Read a big-endian `u16` at an offset.
fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    match *buf.get(offset..end)? {
        [hi, lo] => Some(u16::from_be_bytes([hi, lo])),
        _ => None,
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
        let value = read_u16(self.buf, self.pos)?;
        self.pos = self.pos.saturating_add(2);
        Some(value)
    }

    /// A length-prefixed byte string.
    fn string(&mut self) -> Option<&'a [u8]> {
        let len = usize::from(read_u16(self.buf, self.pos)?);
        let start = self.pos.saturating_add(2);
        let end = start.checked_add(len)?;
        let value = self.buf.get(start..end)?;
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
}

#[cfg(test)]
mod tests {
    use super::{CodecError, Connect, Packet, Publish, QoS, Subscribe, decode_varint, write_varint};

    const SERIAL: &str = "0EXAMPLE00000001";

    /// A string's length as the 16-bit prefix MQTT uses.
    ///
    /// Every string in these tests is a short literal, so the conversion cannot fail — but saying so
    /// with `try_from` rather than `as` keeps the truncating-cast lint meaningful everywhere else.
    fn len16(value: &str) -> u16 {
        u16::try_from(value.len()).expect("test strings are short")
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

        let mut packet = vec![0x10];
        write_varint(&mut packet, body.len());
        packet.extend_from_slice(&body);
        packet
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
            }) => {
                assert_eq!(protocol_level, 4);
                assert_eq!(client_id, SERIAL);
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
        let mut wire = vec![0x32]; // PUBLISH, QoS 1
        write_varint(&mut wire, body.len());
        wire.extend_from_slice(&body);

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
        let mut wire = vec![0x82];
        write_varint(&mut wire, body.len());
        wire.extend_from_slice(&body);

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
        let mut wire = vec![0x34];
        write_varint(&mut wire, 5);
        wire.extend_from_slice(&[0x00, 0x01, b'x', 0x00, 0x01]);
        assert!(matches!(
            Packet::decode(&wire),
            Err(CodecError::UnsupportedQoS { qos: 2 })
        ));
    }

    #[test]
    fn varints_round_trip_across_their_boundaries() {
        for value in [0usize, 1, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152] {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            let (decoded, len) = decode_varint(&out).expect("decode").expect("complete");
            assert_eq!(decoded, value, "value {value}");
            assert_eq!(len, out.len(), "value {value}");
        }
    }

    #[test]
    fn an_overlong_varint_is_rejected() {
        assert_eq!(
            decode_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            Err(CodecError::MalformedLength)
        );
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
        let mut wire = vec![0x32];
        write_varint(&mut wire, body.len());
        wire.extend_from_slice(&body);

        assert!(body.len() > 127, "the length must need two octets");
        let (packet, used) = Packet::decode(&wire).expect("decode").expect("complete");
        assert_eq!(used, wire.len());
        match packet {
            Packet::Publish(p) => assert_eq!(p.payload.len(), 585),
            other => panic!("expected PUBLISH, got {}", other.kind()),
        }
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
