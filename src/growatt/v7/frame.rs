//! Frame layout, obfuscation and integrity.
//!
//! Every MQTT PUBLISH payload, in both directions, is one frame:
//!
//! ```text
//! 0      2        4        6   7      8                    38          len-2
//! +------+--------+--------+---+------+--------------------+-----------+-----+
//! | txid | proto  | length |ad |func  | device id (30)     | body      | crc |
//! +------+--------+--------+---+------+--------------------+-----------+-----+
//! |<-------- clear ------------------>|<---- obfuscated -------------->|clear|
//! ```
//!
//! All multi-octet integers are big-endian, including the CRC — which differs from Modbus RTU
//! convention.
//!
//! # Order of operations
//!
//! Building a frame is: assemble plaintext, obfuscate the body, compute the CRC **over the
//! obfuscated octets**, append it in clear. The CRC is not itself obfuscated. Getting this wrong
//! produces frames the device silently rejects, so [`Frame`] never stores a CRC — [`Frame::to_wire`]
//! recomputes it every time, and it is the only place that can.

use crc::{CRC_16_MODBUS, Crc};

use crate::growatt::ProtocolVersion;
use crate::growatt::header::{self, Header};
use crate::model::Raw;

/// The obfuscation key: a fixed, publicly known constant.
///
/// This is obfuscation, not encryption. It provides no confidentiality whatsoever.
pub const OBFUSCATION_KEY: &[u8] = b"Growatt";

/// The generation this codec implements.
pub const VERSION: ProtocolVersion = ProtocolVersion::V7;

/// Size of the cleartext header.
pub const HEADER_LEN: usize = header::LEN;

/// Offset of the device ID field.
pub const DEVICE_ID_OFFSET: usize = HEADER_LEN;

/// Size of the device ID field: a 16-character serial, NUL-padded to 30 octets.
///
/// Easy to mis-read as "16-byte serial followed by 14 bytes of padding", and then to treat the body
/// as starting at 24. It is one fixed 30-octet field.
pub const DEVICE_ID_LEN: usize = 30;

/// Offset at which the function-specific body begins.
pub const BODY_OFFSET: usize = DEVICE_ID_OFFSET + DEVICE_ID_LEN;

/// Size of the trailing CRC.
pub const CRC_LEN: usize = 2;

/// Shortest frame that can exist: header, device ID and CRC, with an empty body.
pub const MIN_FRAME_LEN: usize = BODY_OFFSET + CRC_LEN;

/// Transaction ID sent in frames this implementation originates.
///
/// Every observed frame in either direction carried `0x0001`; the field's purpose is unconfirmed.
pub const DEFAULT_TRANSACTION_ID: u16 = 1;

const CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_MODBUS);

/// Something wrong with the octets on the wire.
///
/// A leaf error: it describes the bytes and nothing about the surrounding operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// Fewer octets than the minimum frame.
    #[error("frame is {len} octets, below the {MIN_FRAME_LEN}-octet minimum")]
    TooShort {
        /// Length received.
        len: usize,
    },

    /// The frame declares a protocol generation this codec does not implement.
    #[error("protocol generation is {found}, this codec implements {VERSION}")]
    WrongVersion {
        /// Generation declared by the frame.
        found: ProtocolVersion,
    },

    /// The length field disagrees with the octets actually present.
    #[error("length field is {declared} but the frame implies {expected} ({actual} octets total)")]
    BadLength {
        /// Value of the length field.
        declared: u16,
        /// What it should have been.
        expected: usize,
        /// Total octets received.
        actual: usize,
    },

    /// The CRC did not match.
    #[error("CRC mismatch: frame carries {carried:#06x}, computed {computed:#06x}")]
    BadCrc {
        /// CRC present in the frame.
        carried: u16,
        /// CRC computed over the received octets.
        computed: u16,
    },

    /// The device ID field was not printable ASCII.
    #[error("device id is not printable ASCII")]
    BadDeviceId,

    /// A device ID too long for the fixed field.
    #[error("device id is {len} octets, longer than the {DEVICE_ID_LEN}-octet field")]
    DeviceIdTooLong {
        /// Length supplied.
        len: usize,
    },
}

/// Address and function read together as a single message type.
///
/// The two fields are better dispatched on together than separately: under this reading the
/// `0xFE`-prefixed messages are not out-of-range Modbus functions but a separate datalogger-scoped
/// message space.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum MessageType {
    /// `0x0104` — periodic telemetry, device to server.
    Telemetry,
    /// `0x0103` — hourly settings snapshot, device to server.
    SettingsSnapshot,
    /// `0x0105` — on-demand read of one register, either direction.
    ReadSingleRegister,
    /// `0x0106` — write one register, server to device.
    WriteSingleRegister,
    /// `0x0110` — write a contiguous register range, server to device.
    WriteRegisterRange,
    /// `0x0150` — extended telemetry, device to server.
    ExtendedTelemetry,
    /// `0xFE18` — server time push, server to device.
    TimePush,
    /// `0xFE19` — datalogger identity report, device to server.
    IdentityReport,
    /// Anything else. Log it with a hex dump rather than dropping it: that is how the next unknown
    /// message type gets characterised.
    Unrecognised {
        /// Modbus unit address.
        address: u8,
        /// Function code.
        function: u8,
    },
}

impl MessageType {
    /// Classify an address and function pair.
    pub const fn from_parts(address: u8, function: u8) -> Self {
        match (address, function) {
            (0x01, 0x03) => Self::SettingsSnapshot,
            (0x01, 0x04) => Self::Telemetry,
            (0x01, 0x05) => Self::ReadSingleRegister,
            (0x01, 0x06) => Self::WriteSingleRegister,
            (0x01, 0x10) => Self::WriteRegisterRange,
            (0x01, 0x50) => Self::ExtendedTelemetry,
            (0xFE, 0x18) => Self::TimePush,
            (0xFE, 0x19) => Self::IdentityReport,
            (address, function) => Self::Unrecognised { address, function },
        }
    }

    /// The address octet.
    pub const fn address(self) -> u8 {
        match self {
            Self::SettingsSnapshot
            | Self::Telemetry
            | Self::ReadSingleRegister
            | Self::WriteSingleRegister
            | Self::WriteRegisterRange
            | Self::ExtendedTelemetry => 0x01,
            Self::TimePush | Self::IdentityReport => 0xFE,
            Self::Unrecognised { address, .. } => address,
        }
    }

    /// The function octet.
    pub const fn function(self) -> u8 {
        match self {
            Self::SettingsSnapshot => 0x03,
            Self::Telemetry => 0x04,
            Self::ReadSingleRegister => 0x05,
            Self::WriteSingleRegister => 0x06,
            Self::WriteRegisterRange => 0x10,
            Self::ExtendedTelemetry => 0x50,
            Self::TimePush => 0x18,
            Self::IdentityReport => 0x19,
            Self::Unrecognised { function, .. } => function,
        }
    }

    /// Address and function as one 16-bit value, e.g. `0x0104` for telemetry.
    pub const fn as_u16(self) -> u16 {
        u16::from_be_bytes([self.address(), self.function()])
    }
}

impl core::fmt::Display for MessageType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match *self {
            Self::Telemetry => "telemetry",
            Self::SettingsSnapshot => "settings-snapshot",
            Self::ReadSingleRegister => "read-single",
            Self::WriteSingleRegister => "write-single",
            Self::WriteRegisterRange => "write-range",
            Self::ExtendedTelemetry => "extended-telemetry",
            Self::TimePush => "time-push",
            Self::IdentityReport => "identity",
            Self::Unrecognised { .. } => "unrecognised",
        };
        write!(f, "{name}({:#06x})", self.as_u16())
    }
}

/// A parsed frame, held deobfuscated.
///
/// The whole frame is kept rather than just the body, so that the absolute offsets the protocol is
/// specified in — register `n` at `0x4F + 2n`, timestamp at 68 — apply directly. Rebasing them onto
/// a body-relative slice is an easy and silent off-by-30.
///
/// The stored octets exclude the CRC, because a stored CRC could go stale. [`Frame::to_wire`]
/// computes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// Header plus device ID plus body, with the body deobfuscated. No CRC.
    plain: Vec<u8>,
    /// The device ID with padding stripped, validated as ASCII during parsing.
    device_id: Box<str>,
}

impl Frame {
    /// Parse and validate a frame as received on the wire.
    ///
    /// Checks the protocol field, the length rule and the CRC, in that order, then deobfuscates.
    /// The CRC must be verified before deobfuscation because it covers the obfuscated octets.
    ///
    /// # Errors
    ///
    /// One of [`FrameError::TooShort`], [`FrameError::WrongVersion`], [`FrameError::BadLength`],
    /// [`FrameError::BadCrc`] or [`FrameError::BadDeviceId`], naming what failed and what was found.
    /// These are distinct on purpose: an unimplemented generation must be distinguishable from a
    /// corrupt frame.
    pub fn parse(wire: &[u8]) -> Result<Self, FrameError> {
        if wire.len() < MIN_FRAME_LEN {
            return Err(FrameError::TooShort { len: wire.len() });
        }

        let header = Header::peek(wire).ok_or(FrameError::TooShort { len: wire.len() })?;
        if header.protocol != VERSION {
            return Err(FrameError::WrongVersion { found: header.protocol });
        }

        if !header.length_matches(wire.len()) {
            return Err(FrameError::BadLength {
                declared: header.length,
                expected: wire.len().saturating_sub(HEADER_LEN),
                actual: wire.len(),
            });
        }

        let split = wire.len().saturating_sub(CRC_LEN);
        let (covered, tail) = wire.split_at(split);
        let carried = read_u16(tail, 0).ok_or(FrameError::TooShort { len: wire.len() })?;
        let computed = CRC16.checksum(covered);
        if carried != computed {
            return Err(FrameError::BadCrc { carried, computed });
        }

        let mut plain = covered.to_vec();
        transform(&mut plain);

        let device_id = device_id_from(&plain)?;
        Ok(Self { plain, device_id })
    }

    /// Assemble a frame from its parts. The CRC and length field are computed.
    ///
    /// # Errors
    ///
    /// [`FrameError::DeviceIdTooLong`] or [`FrameError::BadDeviceId`] if the serial does not fit the
    /// fixed field or is not printable ASCII.
    pub fn new(message_type: MessageType, device_id: &str, body: &[u8]) -> Result<Self, FrameError> {
        if device_id.len() > DEVICE_ID_LEN {
            return Err(FrameError::DeviceIdTooLong { len: device_id.len() });
        }
        if !device_id.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(FrameError::BadDeviceId);
        }

        let total = BODY_OFFSET
            .checked_add(body.len())
            .and_then(|n| n.checked_add(CRC_LEN))
            .ok_or(FrameError::TooShort { len: body.len() })?;
        let length = u16::try_from(total.saturating_sub(HEADER_LEN)).map_err(|_| FrameError::BadLength {
            declared: 0,
            expected: total,
            actual: total,
        })?;

        let header = Header {
            transaction_id: DEFAULT_TRANSACTION_ID,
            protocol: VERSION,
            length,
            address: message_type.address(),
            function: message_type.function(),
        };

        let mut plain = Vec::with_capacity(total.saturating_sub(CRC_LEN));
        plain.extend_from_slice(&header.to_bytes());
        plain.extend_from_slice(device_id.as_bytes());
        plain.resize(BODY_OFFSET, 0);
        plain.extend_from_slice(body);

        Ok(Self {
            plain,
            device_id: device_id.into(),
        })
    }

    /// Serialise for transmission: obfuscate the body, then append the CRC over the result.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut wire = self.plain.clone();
        transform(&mut wire);
        let crc = CRC16.checksum(&wire);
        wire.extend_from_slice(&crc.to_be_bytes());
        wire
    }

    /// The frame header.
    ///
    /// Always present: a `Frame` cannot exist without one having parsed successfully.
    pub fn header(&self) -> Header {
        Header::peek(&self.plain).unwrap_or(Header {
            transaction_id: DEFAULT_TRANSACTION_ID,
            protocol: VERSION,
            length: 0,
            address: 0,
            function: 0,
        })
    }

    /// The transaction ID from the header.
    pub fn transaction_id(&self) -> u16 {
        self.header().transaction_id
    }

    /// The message type, address and function taken together.
    pub fn message_type(&self) -> MessageType {
        let header = self.header();
        MessageType::from_parts(header.address, header.function)
    }

    /// The device serial, with the field's NUL padding stripped.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The whole frame, deobfuscated, excluding the CRC.
    ///
    /// Offsets into this slice are the absolute offsets the protocol is specified in.
    pub fn plain(&self) -> &[u8] {
        &self.plain
    }

    /// The function-specific body, i.e. everything after the device ID.
    pub fn body(&self) -> &[u8] {
        self.plain.get(BODY_OFFSET..).unwrap_or(&[])
    }

    /// Total length this frame occupies on the wire, CRC included.
    pub fn wire_len(&self) -> usize {
        self.plain.len().saturating_add(CRC_LEN)
    }

    /// Read a big-endian 16-bit value at an absolute frame offset.
    ///
    /// Returns `None` rather than panicking when the offset is out of range: frames from a device
    /// that cannot be patched are untrusted input.
    pub fn u16_at(&self, offset: usize) -> Option<Raw> {
        read_u16(&self.plain, offset).map(Raw)
    }

    /// Read `len` octets at an absolute frame offset.
    pub fn bytes_at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        self.plain.get(offset..end)
    }
}

#[expect(
    clippy::missing_fields_in_debug,
    reason = "the octets are omitted on purpose; use plain() to dump them deliberately"
)]
impl core::fmt::Debug for Frame {
    /// Deliberately terse. A derived implementation would dump 585 octets into every log line that
    /// happens to include a frame.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("type", &self.message_type())
            .field("device_id", &self.device_id)
            .field("wire_len", &self.wire_len())
            .finish()
    }
}

/// Obfuscate or deobfuscate in place; the operation is its own inverse.
///
/// Takes a frame **without** its CRC and covers everything from offset 8 to the end. Expressing it
/// this way rather than as "8 to len − 2" is deliberate: the earlier form silently left two octets
/// untransformed when handed a slice that had already had the CRC split off, which is exactly what
/// parsing does. The CRC never reaches this function, so it cannot be obfuscated by accident.
///
/// The key phase restarts at offset 8.
fn transform(plain_without_crc: &mut [u8]) {
    if let Some(region) = plain_without_crc.get_mut(HEADER_LEN..) {
        for (octet, key) in region.iter_mut().zip(OBFUSCATION_KEY.iter().cycle()) {
            *octet ^= *key;
        }
    }
}

/// Read a big-endian `u16`, or `None` if it would run off the end.
fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let pair = buf.get(offset..end)?;
    match *pair {
        [hi, lo] => Some(u16::from_be_bytes([hi, lo])),
        _ => None,
    }
}

/// Extract and validate the device ID from a deobfuscated frame.
fn device_id_from(plain: &[u8]) -> Result<Box<str>, FrameError> {
    let end = DEVICE_ID_OFFSET
        .checked_add(DEVICE_ID_LEN)
        .ok_or(FrameError::BadDeviceId)?;
    let field = plain.get(DEVICE_ID_OFFSET..end).ok_or(FrameError::BadDeviceId)?;
    let trimmed: Vec<u8> = field.iter().copied().take_while(|b| *b != 0).collect();
    if trimmed.is_empty() || !trimmed.iter().all(u8::is_ascii_graphic) {
        return Err(FrameError::BadDeviceId);
    }
    String::from_utf8(trimmed)
        .map(Into::into)
        .map_err(|_| FrameError::BadDeviceId)
}

#[cfg(test)]
mod tests {
    use super::{
        BODY_OFFSET, CRC16, DEVICE_ID_LEN, Frame, FrameError, MIN_FRAME_LEN, MessageType, OBFUSCATION_KEY, transform,
    };
    use crate::growatt::ProtocolVersion;

    const SERIAL: &str = "0EXAMPLE00000001";

    #[test]
    fn transform_is_its_own_inverse() {
        let original: Vec<u8> = (0u8..=200).collect();
        let mut buf = original.clone();
        transform(&mut buf);
        assert_ne!(buf, original, "obfuscation changed nothing");
        transform(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn transform_leaves_the_header_alone() {
        let mut buf = vec![0xAA; 38];
        transform(&mut buf);
        assert_eq!(&buf[..8], &[0xAA; 8], "header must stay in clear");
        assert_ne!(buf[8], 0xAA, "body must be transformed");
        assert_ne!(buf[37], 0xAA, "the last body octet must be transformed too");
    }

    #[test]
    fn the_crc_is_not_obfuscated() {
        // The regression this guards: `transform` used to be specified as "offsets 8 to len − 2",
        // which quietly left the final two body octets untouched when given a slice that had already
        // had the CRC removed. The round trip below is what surfaced it.
        let body = [0xFFu8; 8];
        let frame = Frame::new(MessageType::Telemetry, SERIAL, &body).expect("build");
        let wire = frame.to_wire();
        let split = wire.len() - 2;

        // Recomputing over the wire octets must reproduce the trailing two, in clear and big-endian.
        let crc = CRC16.checksum(&wire[..split]);
        assert_eq!(&wire[split..], &crc.to_be_bytes());

        // And every body octet, including the last, must differ from the plaintext.
        let plain = frame.plain();
        assert_eq!(plain.len(), split);
        assert!(
            plain[8..].iter().zip(&wire[8..split]).all(|(p, w)| p != w),
            "an octet of the body survived unobfuscated"
        );
    }

    #[test]
    fn key_phase_restarts_at_the_body() {
        let mut buf = vec![0u8; MIN_FRAME_LEN];
        transform(&mut buf);
        // Zero XOR key is the key, which is why an obfuscated frame is full of "Growatt".
        assert_eq!(&buf[8..15], OBFUSCATION_KEY);
    }

    #[test]
    fn round_trip_through_the_wire() {
        let body = [0x01, 0x42, 0x01, 0x42, 0x03, 0x20];
        let frame = Frame::new(MessageType::ReadSingleRegister, SERIAL, &body).expect("build");
        let wire = frame.to_wire();
        let parsed = Frame::parse(&wire).expect("parse what we built");
        assert_eq!(parsed, frame);
        assert_eq!(parsed.device_id(), SERIAL);
        assert_eq!(parsed.body(), body);
        assert_eq!(parsed.message_type(), MessageType::ReadSingleRegister);
    }

    #[test]
    fn length_field_follows_the_rule() {
        let frame = Frame::new(MessageType::WriteSingleRegister, SERIAL, &[0, 1, 0, 2]).expect("b");
        let wire = frame.to_wire();
        let declared = u16::from_be_bytes([wire[4], wire[5]]);
        assert_eq!(usize::from(declared), wire.len() - 8);
        assert_eq!(wire.len(), 44, "single-register write is 44 octets");
    }

    #[test]
    fn rejects_a_short_frame() {
        assert_eq!(Frame::parse(&[0; 12]), Err(FrameError::TooShort { len: 12 }));
    }

    #[test]
    fn rejects_another_generation() {
        let mut wire = Frame::new(MessageType::Telemetry, SERIAL, &[0; 4])
            .expect("build")
            .to_wire();
        wire[3] = 6;
        // Distinguishable from a malformed frame, which is the point of the version field.
        assert!(matches!(
            Frame::parse(&wire),
            Err(FrameError::WrongVersion { found }) if found == ProtocolVersion(6)
        ));
    }

    #[test]
    fn rejects_a_corrupted_body() {
        let mut wire = Frame::new(MessageType::Telemetry, SERIAL, &[0; 8])
            .expect("build")
            .to_wire();
        wire[40] ^= 0xFF;
        assert!(matches!(Frame::parse(&wire), Err(FrameError::BadCrc { .. })));
    }

    #[test]
    fn rejects_a_length_field_that_disagrees() {
        let frame = Frame::new(MessageType::Telemetry, SERIAL, &[0; 8]).expect("build");
        let mut wire = frame.to_wire();
        wire[5] = wire[5].wrapping_add(1);
        // Recompute the CRC so that the length check is what fails, not the CRC.
        let split = wire.len() - 2;
        let crc = CRC16.checksum(&wire[..split]);
        wire[split..].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(Frame::parse(&wire), Err(FrameError::BadLength { .. })));
    }

    #[test]
    fn rejects_an_over_long_device_id() {
        let long = "X".repeat(DEVICE_ID_LEN + 1);
        assert!(matches!(
            Frame::new(MessageType::Telemetry, &long, &[]),
            Err(FrameError::DeviceIdTooLong { .. })
        ));
    }

    #[test]
    fn device_id_is_a_single_thirty_octet_field() {
        let frame = Frame::new(MessageType::Telemetry, SERIAL, &[0xEE; 4]).expect("build");
        // The classic misreading is that the body starts at 24, right after the 16-char serial.
        assert_eq!(BODY_OFFSET, 38);
        assert_eq!(frame.body(), [0xEE; 4]);
        assert_eq!(frame.plain().get(24..38), Some([0u8; 14].as_slice()));
    }

    #[test]
    fn message_types_round_trip_through_their_parts() {
        for expected in [
            MessageType::Telemetry,
            MessageType::SettingsSnapshot,
            MessageType::ReadSingleRegister,
            MessageType::WriteSingleRegister,
            MessageType::WriteRegisterRange,
            MessageType::ExtendedTelemetry,
            MessageType::TimePush,
            MessageType::IdentityReport,
        ] {
            let parsed = MessageType::from_parts(expected.address(), expected.function());
            assert_eq!(parsed, expected, "{expected} did not survive a round trip");
        }
        assert_eq!(MessageType::Telemetry.as_u16(), 0x0104);
        assert_eq!(MessageType::IdentityReport.as_u16(), 0xFE19);
    }

    #[test]
    fn unrecognised_types_keep_their_octets() {
        let mystery = MessageType::from_parts(0x02, 0x64);
        assert_eq!(
            mystery,
            MessageType::Unrecognised {
                address: 0x02,
                function: 0x64
            }
        );
        assert_eq!(mystery.as_u16(), 0x0264);
    }
}
