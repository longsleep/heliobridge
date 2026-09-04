//! Turning a generation-7 frame into values.

use snafu::{OptionExt, Snafu, ensure};

use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::v7::registers::{HoldingRegister, INPUT_REGISTERS, InputRegister, Kind};
use crate::model::{Raw, Reading, Register, Value};

/// Absolute offset of the six-octet timestamp.
pub const TIMESTAMP_OFFSET: usize = 68;

/// Absolute offset of the record-type marker.
pub const RECORD_MARKER_OFFSET: usize = 74;

/// Record-type marker value carried by telemetry, both `0x04` and `0x50`.
pub const RECORD_MARKER_TELEMETRY: u8 = 0x02;

/// Why a frame could not be decoded.
///
/// Uses `snafu` because these cross a module boundary: the caller wants to know which register or
/// offset failed, not merely that decoding failed.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(visibility(pub))]
pub enum DecodeError {
    /// The frame is not the message type this decoder handles.
    #[snafu(display("expected {expected} but the frame is {actual}"))]
    WrongMessageType {
        /// What the decoder wanted.
        expected: MessageType,
        /// What it was given.
        actual: MessageType,
    },

    /// A field extended past the end of the frame.
    #[snafu(display("field {field} needs offsets {offset}..{end} but the frame holds {available} octets"))]
    Truncated {
        /// Name of the field being read.
        field: &'static str,
        /// Where the read started.
        offset: usize,
        /// Where it would have ended.
        end: usize,
        /// Octets actually available.
        available: usize,
    },

    /// A snapshot declared a register range that runs backwards.
    #[snafu(display("snapshot declares registers {start}..{end}, which runs backwards"))]
    Range {
        /// First register declared.
        start: Register,
        /// Last register declared.
        end: Register,
    },

    /// Text did not decode as ASCII.
    #[snafu(display("field {field} at offset {offset} is not printable ASCII"))]
    NotAscii {
        /// Name of the field.
        field: &'static str,
        /// Where it started.
        offset: usize,
    },

    /// A read response named two different registers.
    #[snafu(display("read response names register {register} then echoes {echoed}"))]
    MismatchedEcho {
        /// The first copy.
        register: Register,
        /// The second copy.
        echoed: Register,
    },
}

/// A device-reported wall-clock time.
///
/// Re-exported from [`crate::model`] rather than defined here: a wall-clock time is not specific to a
/// protocol generation, and anything that produces or consumes one — the clock that feeds the time
/// push, most obviously — must not have to depend on this module to do it.
pub use crate::model::Timestamp;

/// A decoded telemetry frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Telemetry {
    /// Device serial from the frame header.
    pub device_id: String,
    /// Device clock, or `None` when the frame carried an all-zero timestamp.
    ///
    /// The device does send all-zero occasionally, on schedule and with every other field populated.
    /// A receiver must tolerate it and fall back to its own clock.
    pub timestamp: Option<Timestamp>,
    /// The record-type marker, which classifies the payload independently of the function code.
    pub record_marker: u8,
    /// One entry per documented register found in the frame.
    pub readings: Vec<Reading>,
}

impl Telemetry {
    /// Find a reading by field name.
    pub fn get(&self, name: &str) -> Option<&Reading> {
        self.readings.iter().find(|r| r.name == name)
    }

    /// A field's numeric value, if present and numeric.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.get(name)?.as_f64()
    }

    /// Battery charge power as the vendor cloud computes it: `pv_power_total − |ac_power|`.
    ///
    /// Some quantities are derived rather than transmitted. This one is also reported directly in
    /// register 11, and the two agree on every captured frame — so this exists to cross-check the
    /// decode, and as the fallback if a firmware revision stops populating the register.
    pub fn derived_battery_charge_power(&self) -> Option<f64> {
        let pv = self.value("pv_power_total")?;
        let ac = self.value("ac_power")?;
        Some(pv - ac.abs())
    }

    /// The device serial as reported inside the register block, reassembled from its four parts.
    ///
    /// A frame carries the serial three times: twice in the header area and once here. They agree,
    /// so this is only useful as a consistency check.
    pub fn embedded_serial(&self) -> Option<String> {
        let mut out = String::new();
        for part in 1..=4 {
            let name = match part {
                1 => "serial_number_part_1",
                2 => "serial_number_part_2",
                3 => "serial_number_part_3",
                _ => "serial_number_part_4",
            };
            match &self.get(name)?.value {
                Value::Text(text) => out.push_str(text),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// The answer to a single-register read.
///
/// Body layout is the register number **twice** followed by the value, mirroring the request. Both copies
/// are checked against each other: they have always agreed, and a disagreement would mean the response
/// belongs to a different read than the one being awaited — which matters when reads are issued in
/// sequence.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ReadResponse {
    /// Which register was read.
    pub register: Register,
    /// Its value, unscaled.
    pub raw: Raw,
}

impl FromFrame for ReadResponse {
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        let actual = frame.message_type();
        ensure!(
            actual == MessageType::ReadSingleRegister,
            WrongMessageTypeSnafu {
                expected: MessageType::ReadSingleRegister,
                actual,
            }
        );

        let body = frame.body();
        let field = |offset: usize, field| {
            let end = offset.saturating_add(2);
            body.get(offset..end)
                .and_then(|pair| <[u8; 2]>::try_from(pair).ok())
                .map(u16::from_be_bytes)
                .context(TruncatedSnafu {
                    field,
                    offset,
                    end,
                    available: body.len(),
                })
        };

        let register = field(0, "register")?;
        let echoed = field(2, "register echo")?;
        let raw = field(4, "value")?;

        ensure!(
            register == echoed,
            MismatchedEchoSnafu {
                register: Register(register),
                echoed: Register(echoed),
            }
        );

        Ok(Self {
            register: Register(register),
            raw: Raw(raw),
        })
    }
}

impl TryFrom<&Frame> for ReadResponse {
    type Error = DecodeError;

    fn try_from(frame: &Frame) -> Result<Self, Self::Error> {
        Self::from_frame(frame)
    }
}

/// The device's answer to a write.
///
/// Both write forms are acknowledged, and they say different amounts:
///
/// - **`0x06`**, single register: `<register:2> <status:1> <value:2>`. On acceptance the status is `0x00`
///   and the value is what the register now holds; a refusal was observed as status `0x02` with `0x0001`
///   in place of the value.
/// - **`0x10`**, range: `<start:2> <end:2> <status:1>`. No value at all — and the status was `0x00` even
///   for a write the device clamped from 1000 to 800.
///
/// That asymmetry is the point. A `0x10` acknowledgement cannot confirm anything, because it says the same
/// thing whether or not the value survived; reading the register back is the only way to know. A `0x06`
/// acknowledgement is more informative, but the stored value it reports has only been seen for writes that
/// were accepted verbatim, so it is treated as a signal rather than as proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAck {
    /// First register written.
    pub start: Register,
    /// Last register written. Equal to `start` for a single-register write.
    pub end: Register,
    /// Status octet. `0x00` accepted; `0x02` observed on a refusal.
    pub status: u8,
    /// The value the device reports holding, for a single-register acknowledgement.
    pub value: Option<Raw>,
}

impl WriteAck {
    /// Whether the device reported accepting the write.
    ///
    /// Not the same as the value having been stored: a range acknowledgement reports acceptance even when
    /// the value was clamped.
    pub const fn accepted(&self) -> bool {
        self.status == 0
    }
}

impl FromFrame for WriteAck {
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        let actual = frame.message_type();
        let body = frame.body();
        let at = |offset: usize, field| {
            let end = offset.saturating_add(2);
            body.get(offset..end)
                .and_then(|pair| <[u8; 2]>::try_from(pair).ok())
                .map(u16::from_be_bytes)
                .context(TruncatedSnafu {
                    field,
                    offset,
                    end,
                    available: body.len(),
                })
        };
        let octet = |offset: usize, field| {
            body.get(offset).copied().context(TruncatedSnafu {
                field,
                offset,
                end: offset.saturating_add(1),
                available: body.len(),
            })
        };

        match actual {
            MessageType::WriteSingleRegister => {
                let register = Register(at(0, "register")?);
                Ok(Self {
                    start: register,
                    end: register,
                    status: octet(2, "status")?,
                    value: Some(Raw(at(3, "value")?)),
                })
            }
            MessageType::WriteRegisterRange => Ok(Self {
                start: Register(at(0, "start register")?),
                end: Register(at(2, "end register")?),
                status: octet(4, "status")?,
                value: None,
            }),
            actual => WrongMessageTypeSnafu {
                expected: MessageType::WriteSingleRegister,
                actual,
            }
            .fail(),
        }
    }
}

/// A message this codec can decode out of a frame.
///
/// A trait rather than a free function per message type, because there are several to come — the
/// `0x03` settings snapshot and the `0x19` identity report decode from the same frames with the same
/// error type, and each should be reachable the same way.
pub trait FromFrame: Sized {
    /// Decode from a parsed frame.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::WrongMessageType`] if the frame is a different message, or
    /// [`DecodeError::Truncated`] if a fixed-offset field runs past the end of it.
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError>;
}

/// A server-originated write to one or more configuration registers.
///
/// The body is a counted list of assignments, and the value of each is **ASCII text whatever the field
/// means** — a port arrives as `"7006"`. Only the vendor's clock push and its firmware-update campaign have
/// been seen using it, the first with one entry and the second likewise, but the count is real and the
/// vendor's own application writes four entries at once when switching to static addressing.
///
/// Decoded rather than merely classified because the value is the interesting part: which register a cloud
/// write targets says little, and what it puts there says everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWrite {
    /// Every assignment the message carries, in order.
    pub entries: Vec<ConfigAssignment>,
}

/// One assignment out of a [`ConfigWrite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAssignment {
    /// The configuration register being written.
    pub register: Register,
    /// The value, as the text it arrives as. Trailing NULs are stripped; nothing else is interpreted.
    pub value: String,
}

impl FromFrame for ConfigWrite {
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        let actual = frame.message_type();
        ensure!(
            matches!(actual, MessageType::ConfigWrite),
            WrongMessageTypeSnafu {
                expected: MessageType::ConfigWrite,
                actual,
            }
        );

        let body = frame.body();
        let pair = |offset: usize, field| {
            let end = offset.saturating_add(2);
            body.get(offset..end)
                .and_then(|pair| <[u8; 2]>::try_from(pair).ok())
                .map(u16::from_be_bytes)
                .context(TruncatedSnafu {
                    field,
                    offset,
                    end,
                    available: body.len(),
                })
        };

        let count = pair(0, "count")?;
        let mut entries = Vec::with_capacity(usize::from(count));
        let mut at = 2usize;
        for _ in 0..count {
            // Entry length counts the register, the value length and the value: `value + 4`. It is read
            // rather than assumed so a longer entry shape skips correctly instead of desynchronising.
            let entry_len = usize::from(pair(at, "entry length")?);
            let register = Register(pair(at.saturating_add(2), "register")?);
            let value_len = usize::from(pair(at.saturating_add(4), "value length")?);
            let start = at.saturating_add(6);
            let end = start.saturating_add(value_len);
            let raw = body.get(start..end).context(TruncatedSnafu {
                field: "value",
                offset: start,
                end,
                available: body.len(),
            })?;
            entries.push(ConfigAssignment {
                register,
                value: String::from_utf8_lossy(raw).trim_end_matches('\0').to_owned(),
            });
            at = at
                .saturating_add(2)
                .saturating_add(entry_len.max(value_len.saturating_add(4)));
        }
        Ok(Self { entries })
    }
}

/// The hourly settings snapshot: the device's own view of its holding registers.
///
/// The device sends this unprompted, about once an hour, and it is the only message that reports the
/// whole settings space at once. Everything else about settings is read one register at a time, which is
/// why this matters: a change made in the vendor application, or by anything else holding the device's
/// credentials, appears here and nowhere else short of a reconnect.
///
/// # The frame declares its own range
///
/// Two octet pairs before the register block hold the first and last register carried, so the mapping is
/// read from the frame rather than assumed:
///
/// ```text
/// offset(register n) = 587 + 2 × (n − start)
/// ```
///
/// A register inside that range is **not** necessarily meaningful — the space is a union of per-component
/// banks with gaps between them — so only registers the holding map names are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSnapshot {
    /// First register the frame carries.
    pub start: Register,
    /// Last register the frame carries.
    pub end: Register,
    /// Raw values for the registers in range that the holding map names, in register order.
    pub values: Vec<(Register, Raw)>,
}

/// Absolute offset of the first register number the snapshot declares.
pub const SNAPSHOT_START_OFFSET: usize = 583;

/// Absolute offset of the last register number it declares.
pub const SNAPSHOT_END_OFFSET: usize = 585;

/// Absolute offset of the first register value.
pub const SNAPSHOT_BLOCK_OFFSET: usize = 587;

impl FromFrame for SettingsSnapshot {
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        let actual = frame.message_type();
        ensure!(
            actual == MessageType::SettingsSnapshot,
            WrongMessageTypeSnafu {
                expected: MessageType::SettingsSnapshot,
                actual,
            }
        );

        let word = |offset: usize, field| {
            frame.u16_at(offset).map(Raw::get).context(TruncatedSnafu {
                field,
                offset,
                end: offset.saturating_add(2),
                available: frame.wire_len(),
            })
        };
        let start = word(SNAPSHOT_START_OFFSET, "snapshot start")?;
        let end = word(SNAPSHOT_END_OFFSET, "snapshot end")?;

        // A range that runs backwards, or one longer than the frame could hold, is a frame this decoder
        // does not understand rather than a device fault — report it rather than reading past the end.
        ensure!(
            start <= end,
            RangeSnafu {
                start: Register(start),
                end: Register(end),
            }
        );

        let mut values = Vec::new();
        for number in start..=end {
            if HoldingRegister::lookup(Register(number)).is_none() {
                continue;
            }
            let index = usize::from(number.saturating_sub(start));
            let offset = SNAPSHOT_BLOCK_OFFSET.saturating_add(index.saturating_mul(2));
            // A short frame yields the registers it does carry, like telemetry: the device has been seen
            // declaring a range wider than the block it sent.
            if let Some(raw) = frame.u16_at(offset) {
                values.push((Register(number), raw));
            }
        }

        Ok(Self {
            start: Register(start),
            end: Register(end),
            values,
        })
    }
}

/// A view over the input-register block of a frame.
///
/// Borrows the frame and decodes on demand, so reading one register costs one register. This is the
/// single path through which raw octets become a [`Reading`] — having two, one for the whole block
/// and one for individual registers, is how they drift apart.
#[derive(Debug, Copy, Clone)]
pub struct InputBlock<'a> {
    frame: &'a Frame,
}

impl<'a> InputBlock<'a> {
    /// View the input registers of a frame.
    ///
    /// Does not check the message type: the block is at the same offsets in any frame that carries
    /// one, and [`Telemetry::from_frame`] is where the type check belongs.
    pub const fn new(frame: &'a Frame) -> Self {
        Self { frame }
    }

    /// Decode a single register, or `None` if it is undocumented or out of range.
    pub fn get(self, register: Register) -> Option<Reading> {
        self.read(InputRegister::lookup(register)?)
    }

    /// Decode a known table entry.
    ///
    /// Returns `None` when the frame is too short to hold the field, so a shorter telemetry variant
    /// still yields the fields it does carry.
    pub fn read(self, entry: &'static InputRegister) -> Option<Reading> {
        let offset = entry.offset();
        let mut raw = self.frame.u16_at(offset)?;
        let value = match entry.kind {
            Kind::Text { registers } => {
                let len = usize::from(registers).saturating_mul(2);
                let octets = self.frame.bytes_at(offset, len)?;
                Value::Text(octets.iter().copied().take_while(|b| *b != 0).map(char::from).collect())
            }
            Kind::Float32 => {
                let low = self.frame.u16_at(offset.checked_add(2)?)?;
                let wide = (u32::from(raw.get()) << 16) | u32::from(low.get());
                // `raw` reports the low half, which is the register other maps name and the whole value
                // while the high half is zero. The scaled value is the one to trust either way.
                raw = low;
                Value::Float(entry.scaling.apply_u32(wide))
            }
            Kind::Int | Kind::Float | Kind::Enum(_) => entry.decode(raw),
        };
        Some(Reading {
            register: entry.register,
            name: entry.name,
            raw,
            value,
            unit: entry.unit,
            confidence: entry.confidence,
        })
    }

    /// Decode every documented register the frame is long enough to hold.
    pub fn iter(self) -> impl Iterator<Item = Reading> + use<'a> {
        let block = self;
        INPUT_REGISTERS.iter().filter_map(move |entry| block.read(entry))
    }

    /// Check that any text fields hold printable ASCII.
    ///
    /// Separate from decoding because a non-ASCII serial is a reason to distrust the frame, while a
    /// caller reading one numeric register has no reason to care.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::NotAscii`] naming the first text field that is not printable ASCII.
    pub fn validate_text(self) -> Result<(), DecodeError> {
        for entry in INPUT_REGISTERS {
            if !matches!(entry.kind, Kind::Text { .. }) {
                continue;
            }
            if let Some(Reading {
                value: Value::Text(text),
                ..
            }) = self.read(entry)
                && !text.is_empty()
                && !text.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(DecodeError::NotAscii {
                    field: entry.name,
                    offset: entry.offset(),
                });
            }
        }
        Ok(())
    }
}

impl FromFrame for Telemetry {
    /// Decode a `0x04` telemetry frame, or the `0x50` replay of one.
    ///
    /// Both carry the same record; `0x50` differs only in being a sample taken earlier and held in the
    /// device's archive. The timestamp is what tells them apart, and a caller that merges a `0x50` into
    /// current state will publish stale values after every reconnect.
    ///
    /// Unknown registers are decoded and included; whether to present them is the caller's decision.
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        let actual = frame.message_type();
        ensure!(
            matches!(actual, MessageType::Telemetry | MessageType::BufferedTelemetry),
            WrongMessageTypeSnafu {
                expected: MessageType::Telemetry,
                actual,
            }
        );

        let stamp = frame
            .bytes_at(TIMESTAMP_OFFSET, 6)
            .context(TruncatedSnafu {
                field: "timestamp",
                offset: TIMESTAMP_OFFSET,
                end: TIMESTAMP_OFFSET.saturating_add(6),
                available: frame.plain().len(),
            })?
            .to_vec();

        let timestamp = match *stamp.as_slice() {
            [0, 0, 0, 0, 0, 0] => None,
            [y, mo, d, h, mi, s] => Some(Timestamp {
                year: u16::from(y).saturating_add(2000),
                month: mo,
                day: d,
                hour: h,
                minute: mi,
                second: s,
            }),
            _ => None,
        };

        let record_marker = frame
            .bytes_at(RECORD_MARKER_OFFSET, 1)
            .and_then(|b| b.first().copied())
            .context(TruncatedSnafu {
                field: "record_marker",
                offset: RECORD_MARKER_OFFSET,
                end: RECORD_MARKER_OFFSET.saturating_add(1),
                available: frame.plain().len(),
            })?;

        let block = InputBlock::new(frame);
        block.validate_text()?;

        Ok(Self {
            device_id: frame.device_id().to_owned(),
            timestamp,
            record_marker,
            readings: block.iter().collect(),
        })
    }
}

impl TryFrom<&Frame> for Telemetry {
    type Error = DecodeError;

    fn try_from(frame: &Frame) -> Result<Self, Self::Error> {
        Self::from_frame(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigWrite, FromFrame, RECORD_MARKER_TELEMETRY, Telemetry, Timestamp};
    use crate::growatt::v7::frame::{Frame, MessageType};
    use crate::model::Register;

    /// One config-write entry, as the vendor lays it out: entry length, register, value length, value.
    fn config_entry(register: u16, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let length = u16::try_from(value.len().saturating_add(4)).expect("a short value");
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&register.to_be_bytes());
        out.extend_from_slice(&u16::try_from(value.len()).expect("a short value").to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn config_write(entries: &[(u16, &str)]) -> Frame {
        let mut body = u16::try_from(entries.len())
            .expect("few entries")
            .to_be_bytes()
            .to_vec();
        for (register, value) in entries {
            body.extend_from_slice(&config_entry(*register, value));
        }
        Frame::new(MessageType::ConfigWrite, "0EXAMPLE00000001", &body).expect("build")
    }

    #[test]
    fn the_clock_push_decodes_as_one_assignment() {
        // The specification's own worked example of this message: register 31, a 19-character timestamp.
        let frame = config_write(&[(31, "2026-08-08 02:10:29")]);
        let decoded = ConfigWrite::from_frame(&frame).expect("decodes");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].register, Register(31));
        assert_eq!(decoded.entries[0].value, "2026-08-08 02:10:29");
    }

    #[test]
    fn the_update_campaign_carries_its_url_as_text() {
        // What the vendor's cloud writes to register 80, hourly, verbatim from a capture.
        let url =
            "1#type01#http://cdn.growatt.com/update/device/GB/manualUpgrade/7ca7/1eacd/f37825/R-D/WIFI/4.0.2.6.bin";
        let decoded = ConfigWrite::from_frame(&config_write(&[(80, url)])).expect("decodes");
        assert_eq!(decoded.entries[0].register, Register(80));
        assert_eq!(decoded.entries[0].value, url);
    }

    #[test]
    fn several_assignments_in_one_message_all_decode() {
        // Never seen from the vendor's server, but its own application writes four at once, so the count
        // is not decoration.
        let decoded = ConfigWrite::from_frame(&config_write(&[
            (14, "192.168.5.10"),
            (25, "255.255.255.0"),
            (26, "192.168.5.1"),
            (71, "1"),
        ]))
        .expect("decodes");
        assert_eq!(decoded.entries.len(), 4);
        assert_eq!(decoded.entries[3].register, Register(71));
        assert_eq!(decoded.entries[3].value, "1");
    }

    #[test]
    fn a_truncated_value_is_an_error_rather_than_a_short_string() {
        // A value that runs past the body would otherwise decode as a plausible shorter one.
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&config_entry(80, "http://example.invalid/f.bin"));
        body.truncate(body.len() - 6);
        let frame = Frame::new(MessageType::ConfigWrite, "0EXAMPLE00000001", &body).expect("build");
        assert!(ConfigWrite::from_frame(&frame).is_err());
    }

    #[test]
    fn a_frame_of_another_type_is_refused() {
        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &[0; 8]).expect("build");
        assert!(ConfigWrite::from_frame(&frame).is_err());
    }

    #[test]
    fn rejects_a_frame_of_the_wrong_type() {
        let frame = Frame::new(MessageType::WriteSingleRegister, "0EXAMPLE00000001", &[0; 4]).expect("build");
        let err = Telemetry::from_frame(&frame).expect_err("a write is not telemetry");
        assert!(
            err.to_string().contains("write-single"),
            "error should name the offending type: {err}"
        );
    }

    #[test]
    fn all_zero_timestamp_becomes_none_rather_than_an_error() {
        // The device really does this: one snapshot in seven arrived with a zero timestamp, on
        // schedule, every other field populated. Rejecting the frame would discard good telemetry.
        let mut body = vec![0u8; 547];
        body[RECORD_MARKER_OFFSET_IN_BODY] = RECORD_MARKER_TELEMETRY;
        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &body).expect("build");
        let decoded = Telemetry::from_frame(&frame).expect("decode");
        assert!(decoded.timestamp.is_none());
        assert_eq!(decoded.record_marker, RECORD_MARKER_TELEMETRY);
    }

    /// Offset of the record marker relative to the start of the body, i.e. 74 − 38.
    const RECORD_MARKER_OFFSET_IN_BODY: usize = 36;

    #[test]
    fn a_read_response_decodes_its_register_and_value() {
        use super::ReadResponse;
        use crate::growatt::v7::encode::Command;
        use crate::model::{Raw, Register};

        // The worked example from the specification: reading register 322 answered 0x0320 = 800.
        let body = [0x01, 0x42, 0x01, 0x42, 0x03, 0x20];
        let frame = Frame::new(MessageType::ReadSingleRegister, "0EXAMPLE00000001", &body).expect("build");
        assert_eq!(frame.wire_len(), 46, "a read response is 46 octets");

        let response = ReadResponse::from_frame(&frame).expect("decode");
        assert_eq!(response.register, Register(322));
        assert_eq!(response.raw, Raw(800));

        // And it answers the request the encoder builds for the same register.
        let request = Command::read(Register(322))
            .to_frame("0EXAMPLE00000001")
            .expect("build");
        assert_eq!(request.body().get(..4), frame.body().get(..4));
    }

    #[test]
    fn a_read_response_that_names_two_registers_is_refused() {
        use super::{DecodeError, ReadResponse};

        // The echo exists to be checked. A mismatch means the answer belongs to a different read than the
        // one being awaited, which matters when reads are issued in sequence.
        let body = [0x01, 0x42, 0x01, 0x43, 0x03, 0x20];
        let frame = Frame::new(MessageType::ReadSingleRegister, "0EXAMPLE00000001", &body).expect("build");
        assert!(matches!(
            ReadResponse::from_frame(&frame),
            Err(DecodeError::MismatchedEcho { .. })
        ));
    }

    #[test]
    fn a_truncated_read_response_is_refused() {
        use super::{DecodeError, ReadResponse};

        for len in 0..6usize {
            let body = vec![0u8; len];
            let frame = Frame::new(MessageType::ReadSingleRegister, "0EXAMPLE00000001", &body).expect("b");
            assert!(
                matches!(ReadResponse::from_frame(&frame), Err(DecodeError::Truncated { .. })),
                "a {len}-octet body should not decode"
            );
        }
    }

    #[test]
    fn a_single_register_acknowledgement_carries_a_status_and_the_stored_value() {
        use super::WriteAck;
        use crate::model::{Raw, Register};

        // Octets the device actually sent, answering a write of 100 to slot1_output_power.
        let frame = Frame::new(
            MessageType::WriteSingleRegister,
            "0EXAMPLE00000001",
            &[0x01, 0x01, 0x00, 0x00, 0x64],
        )
        .expect("build");
        assert_eq!(frame.wire_len(), 45, "the acknowledgement is 45 octets");

        let ack = WriteAck::from_frame(&frame).expect("decode");
        assert_eq!(ack.start, Register(257));
        assert_eq!(ack.end, Register(257));
        assert_eq!(ack.status, 0);
        assert_eq!(ack.value, Some(Raw(100)));
        assert!(ack.accepted());
    }

    #[test]
    fn a_refusal_is_distinguishable_from_an_acceptance() {
        use super::WriteAck;
        use crate::model::Register;

        // What the device sent when 1000 W was written to slot1_output_power: status 2, and 0x0001 where
        // an accepted write reports the stored value.
        let frame = Frame::new(
            MessageType::WriteSingleRegister,
            "0EXAMPLE00000001",
            &[0x01, 0x01, 0x02, 0x00, 0x01],
        )
        .expect("build");

        let ack = WriteAck::from_frame(&frame).expect("decode");
        assert_eq!(ack.start, Register(257));
        assert_eq!(ack.status, 2);
        assert!(!ack.accepted());
    }

    #[test]
    fn a_range_acknowledgement_reports_no_value_even_when_the_write_was_clamped() {
        use super::WriteAck;
        use crate::model::Register;

        // The device sent exactly this after storing 800 for a write of 1000: the range, and a status of
        // zero. Nothing distinguishes it from a write that was stored verbatim, which is why a read-back
        // is the only confirmation worth having.
        let frame = Frame::new(
            MessageType::WriteRegisterRange,
            "0EXAMPLE00000001",
            &[0x01, 0x41, 0x01, 0x42, 0x00],
        )
        .expect("build");

        let ack = WriteAck::from_frame(&frame).expect("decode");
        assert_eq!(ack.start, Register(321));
        assert_eq!(ack.end, Register(322));
        assert_eq!(ack.status, 0);
        assert_eq!(ack.value, None, "a range acknowledgement carries no value");
        assert!(ack.accepted(), "and claims success regardless of clamping");
    }

    #[test]
    fn telemetry_is_not_mistaken_for_a_write_acknowledgement() {
        use super::WriteAck;

        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &[0u8; 547]).expect("build");
        assert!(WriteAck::from_frame(&frame).is_err());
    }

    #[test]
    fn telemetry_is_not_mistaken_for_a_read_response() {
        use super::ReadResponse;

        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &[0u8; 547]).expect("build");
        assert!(ReadResponse::from_frame(&frame).is_err());
    }

    #[test]
    fn implausible_timestamps_are_flagged_not_rejected() {
        let good = Timestamp {
            year: 2026,
            month: 8,
            day: 7,
            hour: 22,
            minute: 39,
            second: 51,
        };
        assert!(good.is_plausible());
        assert_eq!(good.to_string(), "2026-08-07 22:39:51");

        let bad = Timestamp { month: 13, ..good };
        assert!(!bad.is_plausible());
    }
}
