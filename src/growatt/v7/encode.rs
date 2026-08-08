//! Building generation-7 frames: writes, reads and the time push.
//!
//! # The allowlist is a type, not a check
//!
//! Writing a speculative value to an unknown register on a mains-connected battery inverter is the one
//! action here with a real safety warning attached. So [`Command`] cannot be constructed with a bare
//! register number: it needs a [`WritableRegister`], which can only be obtained by finding an entry in
//! the holding register map. "Never write to an undocumented register" is therefore unrepresentable
//! rather than merely checked, and no amount of later editing in the layers above can reintroduce it.
//!
//! # Composite writes
//!
//! Two settings are not written as themselves by the vendor server. `default_output_power` arrives as a
//! range write covering registers `321..322` with a zero in the unknown 321, and a schedule slot as a
//! range covering all five of its registers. [`Command::set_default_output_power`] and
//! [`Command::set_slot`] reproduce those exactly, because "behave like the server it replaces" is the
//! whole design, and a single-register write to 322 has never been observed on this hardware.
//!
//! Register 321 is not in the writable map, so nothing can write a nonzero value to it, or write it
//! alone.

use snafu::{OptionExt, ResultExt, Snafu, ensure};

use crate::growatt::v7::decode::Timestamp;
use crate::growatt::v7::frame::{Frame, FrameError, MessageType};
use crate::growatt::v7::registers::{self, Domain, HoldingRegister, SLOT_STRIDE};
use crate::model::{Raw, Register};

/// Register the vendor server writes alongside `default_output_power`, always as zero.
///
/// Not in the writable map. It exists here only so the one composite write that carries it can be
/// reproduced octet for octet.
const COMPANION_OF_DEFAULT_OUTPUT_POWER: u16 = 321;

/// Register carrying `default_output_power`.
const DEFAULT_OUTPUT_POWER: u16 = 322;

/// Register carrying `power_plus`, which gates the ceiling on [`DEFAULT_OUTPUT_POWER`].
const POWER_PLUS: u16 = 325;

/// Fixed prefix of the time push body, ahead of the ASCII timestamp.
///
/// The trailing `00 13` is 19, the length of the string that follows. The preceding pairs are
/// unconfirmed and reproduced verbatim.
pub const TIME_PUSH_PREFIX: [u8; 8] = [0x00, 0x01, 0x00, 0x17, 0x00, 0x1F, 0x00, 0x13];

/// Length of the ASCII timestamp in a time push: `YYYY-MM-DD HH:MM:SS`.
pub const TIME_PUSH_TEXT_LEN: usize = 19;

/// Why a command could not be built.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(visibility(pub))]
pub enum EncodeError {
    /// The register is not one this implementation will write.
    #[snafu(display("register {register} is not writable: it is absent from the holding register map"))]
    NotWritable {
        /// The register asked for.
        register: Register,
    },

    /// The value is outside what the register accepts.
    #[snafu(display("{name} (register {register}) accepts {accepted}, not {value}"))]
    OutOfRange {
        /// Field name.
        name: &'static str,
        /// Register number.
        register: Register,
        /// What the register accepts.
        accepted: String,
        /// The value offered.
        value: u16,
    },

    /// A range write covered registers that are not consecutive.
    #[snafu(display("a range write must be consecutive: {previous} is followed by {register}"))]
    NotConsecutive {
        /// The preceding register.
        previous: Register,
        /// The register that broke the run.
        register: Register,
    },

    /// A range write had no registers in it.
    #[snafu(display("a range write needs at least one register"))]
    EmptyRange,

    /// The slot number is outside the device's schedule.
    #[snafu(display("slot {slot} does not exist; slots are numbered 1 to {available}"))]
    NoSuchSlot {
        /// The slot asked for.
        slot: u16,
        /// How many exist.
        available: u16,
    },

    /// A timestamp that cannot be rendered.
    #[snafu(display("timestamp {timestamp} is not a plausible calendar time"))]
    ImplausibleTimestamp {
        /// The offending value.
        timestamp: Timestamp,
    },

    /// The frame layer rejected the assembled parts.
    #[snafu(display("could not assemble the frame"))]
    Frame {
        /// What the frame layer said.
        source: FrameError,
    },
}

/// A register this implementation is willing to write, with the value already validated.
///
/// Obtainable only from the holding register map, which is what makes the allowlist structural. Holds
/// the map entry so that error messages and logs can name the field without a second lookup.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WritableRegister {
    entry: HoldingRegister,
}

impl WritableRegister {
    /// Find a register in the holding map, or `None` if it is not writable.
    pub fn lookup(register: Register) -> Option<Self> {
        HoldingRegister::lookup(register).map(|entry| Self { entry })
    }

    /// The register number.
    pub const fn register(self) -> Register {
        self.entry.register
    }

    /// The field name.
    pub const fn name(self) -> &'static str {
        self.entry.name
    }

    /// What values this register accepts.
    pub const fn domain(self) -> Domain {
        self.entry.domain
    }

    /// Check a value against the register's domain.
    ///
    /// # Errors
    ///
    /// [`EncodeError::OutOfRange`] naming the field and what it accepts. The device clamps silently
    /// rather than refusing, so a rejection here is more informative than the device's own behaviour.
    pub fn validate(self, value: u16) -> Result<Raw, EncodeError> {
        ensure!(
            self.entry.domain.accepts(value),
            OutOfRangeSnafu {
                name: self.entry.name,
                register: self.entry.register,
                accepted: self.entry.domain.describe(),
                value,
            }
        );
        Ok(Raw(value))
    }
}

/// A time-slot configuration, written as one range covering all five of its registers.
///
/// Not the only way to change a slot. A slot register can be written on its own with
/// [`Command::write`] — `slot{n}_output_power` alone was verified to take effect within about a second,
/// which is what makes a dynamic output-following control loop cheap: it need not restate the window,
/// the mode and the enabled flag on every update.
///
/// Use this when changing the **mode**, or several fields at once. Writing `slot{n}_work_mode` in
/// isolation was never tested, and the vendor server always rewrites the whole slot, so the whole-slot
/// range is the path with observed evidence behind it.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SlotConfig {
    /// Start of the window.
    pub start_hour: u8,
    /// Start of the window.
    pub start_minute: u8,
    /// End of the window.
    pub end_hour: u8,
    /// End of the window.
    pub end_minute: u8,
    /// 0 load first, 1 battery first, 2 smart self-use.
    pub work_mode: u16,
    /// Output target in watts. Not a hard cap: with a full battery and surplus PV the device exports
    /// beyond it regardless.
    pub output_power: u16,
    /// Whether the slot is active.
    pub enabled: bool,
}

/// One field within a schedule slot.
///
/// Slots are five consecutive registers, and this names the position within them so a caller can
/// address a single field without doing the `254 + 5n` arithmetic itself.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum SlotField {
    /// Start of the window, `+0`.
    StartTime,
    /// End of the window, `+1`.
    EndTime,
    /// Work mode, `+2`.
    WorkMode,
    /// Output target in watts, `+3`.
    OutputPower,
    /// Active flag, `+4`.
    Enabled,
}

impl SlotField {
    /// Position of this field within its slot.
    pub const fn offset(self) -> u16 {
        match self {
            Self::StartTime => 0,
            Self::EndTime => 1,
            Self::WorkMode => 2,
            Self::OutputPower => 3,
            Self::Enabled => 4,
        }
    }

    /// The register carrying this field for a given slot, counted from 1.
    pub fn register(self, slot: u16) -> Option<Register> {
        let registers = slot_registers(slot)?;
        registers.get(usize::from(self.offset())).copied()
    }
}

impl SlotConfig {
    /// The five raw register values, in register order.
    fn raw_values(self) -> [u16; 5] {
        [
            (u16::from(self.start_hour) << 8) | u16::from(self.start_minute),
            (u16::from(self.end_hour) << 8) | u16::from(self.end_minute),
            self.work_mode,
            self.output_power,
            u16::from(self.enabled),
        ]
    }
}

/// A frame this implementation originates.
///
/// Construction validates; [`Command::to_frame`] only serialises. Splitting it that way means an
/// invalid command cannot be held, so a caller that wants to report a rejection does not have to wait
/// until it is about to transmit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Write one register with `0x06`. Not acknowledged by the device — read it back.
    WriteSingle {
        /// The register.
        register: WritableRegister,
        /// The validated value.
        value: Raw,
    },

    /// Write a consecutive run of registers with `0x10`. Acknowledged with the range only.
    WriteRange {
        /// First register of the run.
        start: Register,
        /// The validated values, one per register.
        values: Vec<Raw>,
    },

    /// Read one register with `0x05`.
    ///
    /// Any register may be read: reading has no side effect, so the allowlist does not apply.
    ReadSingle {
        /// The register to read.
        register: Register,
    },

    /// Push the server's wall-clock time with `0xFE18`.
    TimePush {
        /// The time to send.
        time: Timestamp,
    },
}

impl Command {
    /// Set a setting, in whatever form the vendor uses for it.
    ///
    /// The entry point a caller should reach for. Most settings are a single-register write, but
    /// `default_output_power` is not: the vendor server always writes it as the `321..322` range, and a bare
    /// single-register write to 322 has never been observed on this hardware. Leaving that choice to the
    /// caller means every caller has to know it, and the one that forgets sends an untested frame to a
    /// mains-connected battery inverter.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NotWritable`] if the register is not in the holding map, or
    /// [`EncodeError::OutOfRange`] if the value is not accepted.
    pub fn set(register: Register, value: u16) -> Result<Self, EncodeError> {
        if register.number() == DEFAULT_OUTPUT_POWER {
            return Self::set_default_output_power(value);
        }
        Self::write(register, value)
    }

    /// Write a single register.
    ///
    /// Prefer [`Command::set`], which picks the form the vendor uses for the register in question.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NotWritable`] if the register is not in the holding map, or
    /// [`EncodeError::OutOfRange`] if the value is not accepted.
    pub fn write(register: Register, value: u16) -> Result<Self, EncodeError> {
        let target = WritableRegister::lookup(register).context(NotWritableSnafu { register })?;
        Ok(Self::WriteSingle {
            register: target,
            value: target.validate(value)?,
        })
    }

    /// Write a consecutive run of registers, each validated against its own domain.
    ///
    /// # Errors
    ///
    /// [`EncodeError::EmptyRange`], [`EncodeError::NotConsecutive`], [`EncodeError::NotWritable`] or
    /// [`EncodeError::OutOfRange`].
    pub fn write_range(pairs: &[(Register, u16)]) -> Result<Self, EncodeError> {
        let (first, _) = *pairs.first().context(EmptyRangeSnafu)?;
        let mut values = Vec::with_capacity(pairs.len());
        let mut expected = first;

        for (register, value) in pairs {
            ensure!(
                *register == expected,
                NotConsecutiveSnafu {
                    previous: Register(expected.number().saturating_sub(1)),
                    register: *register,
                }
            );
            let target = WritableRegister::lookup(*register).context(NotWritableSnafu { register: *register })?;
            values.push(target.validate(*value)?);
            expected = Register(register.number().saturating_add(1));
        }

        Ok(Self::WriteRange { start: first, values })
    }

    /// Set `default_output_power`, as the vendor server does.
    ///
    /// Emits the range write covering `321..322`, with the zero the vendor writes to 321. See the
    /// module documentation for why this is a single operation rather than a write to 322.
    ///
    /// The stored value is clamped to 800 W unless `power_plus` is set, and the device re-clamps on its
    /// own when that flag is later cleared — so the caller **must** read the register back rather than
    /// assume this took effect.
    ///
    /// # Errors
    ///
    /// [`EncodeError::OutOfRange`] if the wattage is outside what register 322 accepts.
    pub fn set_default_output_power(watts: u16) -> Result<Self, EncodeError> {
        let power = Register(DEFAULT_OUTPUT_POWER);
        let target = WritableRegister::lookup(power).context(NotWritableSnafu { register: power })?;
        let validated = target.validate(watts)?;

        Ok(Self::WriteRange {
            start: Register(COMPANION_OF_DEFAULT_OUTPUT_POWER),
            values: vec![Raw(0), validated],
        })
    }

    /// Write a whole schedule slot, `slot` counted from 1.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NoSuchSlot`] or [`EncodeError::OutOfRange`] naming the offending field.
    pub fn set_slot(slot: u16, config: SlotConfig) -> Result<Self, EncodeError> {
        let entries = HoldingRegister::slot(slot).context(NoSuchSlotSnafu {
            slot,
            available: registers::SLOT_COUNT,
        })?;
        let raw = config.raw_values();

        let mut values = Vec::with_capacity(entries.len());
        for (entry, value) in entries.iter().zip(raw) {
            let target = WritableRegister { entry: *entry };
            values.push(target.validate(value)?);
        }

        let start = entries.first().context(EmptyRangeSnafu)?.register;
        Ok(Self::WriteRange { start, values })
    }

    /// Set a slot's output power — the operation a bridge performs most.
    ///
    /// Emits a **single-register** `0x06` write to `257 + 5n`. That path is verified on this hardware:
    /// the write is accepted and takes effect within about a second, without restating the slot's time
    /// window, mode or enabled flag. An output-following control loop can therefore run at whatever rate
    /// it likes for the cost of one four-octet body.
    ///
    /// Not acknowledged by the device, so read the register back before treating it as applied. The
    /// value is also an output *target* rather than a cap: with a full battery and surplus PV the device
    /// exports beyond it regardless.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NoSuchSlot`] if the slot does not exist, or [`EncodeError::OutOfRange`] if the
    /// wattage is outside what the register accepts.
    pub fn set_slot_output_power(slot: u16, watts: u16) -> Result<Self, EncodeError> {
        Self::set_slot_field(slot, SlotField::OutputPower, watts)
    }

    /// Enable or disable a slot with a single-register write.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NoSuchSlot`] if the slot does not exist.
    pub fn set_slot_enabled(slot: u16, enabled: bool) -> Result<Self, EncodeError> {
        Self::set_slot_field(slot, SlotField::Enabled, u16::from(enabled))
    }

    /// Set one field of one slot, addressing it by name rather than by register arithmetic.
    ///
    /// Prefer [`Command::set_slot`] when changing the work mode: writing that field in isolation was
    /// never tested, whereas the whole-slot range write is what the vendor server does.
    ///
    /// # Errors
    ///
    /// [`EncodeError::NoSuchSlot`], [`EncodeError::NotWritable`] or [`EncodeError::OutOfRange`].
    pub fn set_slot_field(slot: u16, field: SlotField, value: u16) -> Result<Self, EncodeError> {
        let register = field.register(slot).context(NoSuchSlotSnafu {
            slot,
            available: registers::SLOT_COUNT,
        })?;
        Self::write(register, value)
    }

    /// Read one register on demand.
    ///
    /// This is the only read the device answers, and the only way to learn a switch position without
    /// waiting up to an hour for the settings snapshot.
    pub const fn read(register: Register) -> Self {
        Self::ReadSingle { register }
    }

    /// Push the server's time.
    ///
    /// # Errors
    ///
    /// [`EncodeError::ImplausibleTimestamp`] if the value would not render as a calendar time.
    pub fn time_push(time: Timestamp) -> Result<Self, EncodeError> {
        ensure!(time.is_plausible(), ImplausibleTimestampSnafu { timestamp: time });
        Ok(Self::TimePush { time })
    }

    /// The message type this command is carried by.
    pub const fn message_type(&self) -> MessageType {
        match *self {
            Self::WriteSingle { .. } => MessageType::WriteSingleRegister,
            Self::WriteRange { .. } => MessageType::WriteRegisterRange,
            Self::ReadSingle { .. } => MessageType::ReadSingleRegister,
            Self::TimePush { .. } => MessageType::TimePush,
        }
    }

    /// Which registers should be read back after this command, and what each was asked to hold.
    ///
    /// A write is never self-confirming on this device: range writes are acknowledged with the register
    /// range and nothing else, single-register writes are not acknowledged at all, and out-of-range values
    /// are silently clamped rather than refused. So the only way to know what was stored is to ask.
    ///
    /// Two cases are not simply "the registers written":
    ///
    /// - **321 is excluded.** The composite `default_output_power` write covers it, but it has no known
    ///   meaning, so there is nothing to compare a read against.
    /// - **325 adds 322.** Setting `power_plus` changes `default_output_power` with no write to it —
    ///   clearing the flag drops a stored 1000 W to 800 W on its own. Verifying only what was written would
    ///   miss that, and a cached power value would then be wrong until something else disturbed it.
    ///
    /// The value is `None` where there is nothing to compare against — only something to learn.
    pub fn registers_to_verify(&self) -> Vec<(Register, Option<Raw>)> {
        let mut out: Vec<(Register, Option<Raw>)> = match self {
            Self::WriteSingle { register, value } => vec![(register.register(), Some(*value))],

            Self::WriteRange { start, values } => values
                .iter()
                .enumerate()
                .filter_map(|(offset, value)| {
                    let number = start.number().checked_add(u16::try_from(offset).ok()?)?;
                    let register = Register(number);
                    // Only registers with a documented meaning; the rest have nothing to compare.
                    HoldingRegister::lookup(register).map(|_| (register, Some(*value)))
                })
                .collect(),

            // Reads and the time push change nothing.
            Self::ReadSingle { .. } | Self::TimePush { .. } => Vec::new(),
        };

        let touches_power_plus = out.iter().any(|(register, _)| register.number() == POWER_PLUS);
        let already_verifying_power = out
            .iter()
            .any(|(register, _)| register.number() == DEFAULT_OUTPUT_POWER);
        if touches_power_plus && !already_verifying_power {
            // What 322 ends up holding depends on what was there before as well as on the new flag, so
            // there is no expected value — only a stale one to replace.
            out.push((Register(DEFAULT_OUTPUT_POWER), None));
        }

        out
    }

    /// Whether the device acknowledges this command.
    ///
    /// A range write is echoed back with its register range — and nothing else, so the acknowledgement
    /// proves receipt and not the stored value. A single-register write is not acknowledged at all.
    /// Either way the value has to be read back.
    pub const fn is_acknowledged(&self) -> bool {
        matches!(*self, Self::WriteRange { .. })
    }

    /// Serialise into a frame for the given device.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Frame`] if the device ID does not fit the fixed field or is not printable ASCII.
    pub fn to_frame(&self, device_id: &str) -> Result<Frame, EncodeError> {
        let body = self.body();
        Frame::new(self.message_type(), device_id, &body).context(FrameSnafu)
    }

    /// The function-specific body.
    fn body(&self) -> Vec<u8> {
        match self {
            Self::WriteSingle { register, value } => {
                let mut body = Vec::with_capacity(4);
                body.extend_from_slice(&register.register().number().to_be_bytes());
                body.extend_from_slice(&value.get().to_be_bytes());
                body
            }

            Self::WriteRange { start, values } => {
                // Inclusive end: N = end − start + 1.
                let last = start
                    .number()
                    .saturating_add(u16::try_from(values.len()).unwrap_or(u16::MAX))
                    .saturating_sub(1);
                let mut body = Vec::with_capacity(4usize.saturating_add(values.len().saturating_mul(2)));
                body.extend_from_slice(&start.number().to_be_bytes());
                body.extend_from_slice(&last.to_be_bytes());
                for value in values {
                    body.extend_from_slice(&value.get().to_be_bytes());
                }
                body
            }

            // The register number twice, which is what the device answers to.
            Self::ReadSingle { register } => {
                let mut body = Vec::with_capacity(4);
                body.extend_from_slice(&register.number().to_be_bytes());
                body.extend_from_slice(&register.number().to_be_bytes());
                body
            }

            Self::TimePush { time } => {
                let mut body = Vec::with_capacity(TIME_PUSH_PREFIX.len().saturating_add(TIME_PUSH_TEXT_LEN));
                body.extend_from_slice(&TIME_PUSH_PREFIX);
                body.extend_from_slice(format_time(*time).as_bytes());
                body
            }
        }
    }
}

/// Render a timestamp as the 19-character form the device is sent.
fn format_time(time: Timestamp) -> String {
    let Timestamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = time;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// The registers of a schedule slot, `slot` counted from 1.
pub fn slot_registers(slot: u16) -> Option<[Register; 5]> {
    let entries = HoldingRegister::slot(slot)?;
    let mut out = [Register(0); 5];
    for (target, entry) in out.iter_mut().zip(entries) {
        *target = entry.register;
    }
    Some(out)
}

/// Stride between consecutive slots, exposed for callers iterating them.
pub const fn slot_stride() -> u16 {
    SLOT_STRIDE
}

#[cfg(test)]
mod tests {
    use super::{Command, EncodeError, SlotConfig, SlotField, WritableRegister, slot_registers};
    use crate::growatt::v7::decode::Timestamp;
    use crate::growatt::v7::frame::MessageType;
    use crate::model::{Raw, Register};

    const SERIAL: &str = "0EXAMPLE00000001";

    fn stamp() -> Timestamp {
        Timestamp {
            year: 2026,
            month: 8,
            day: 6,
            hour: 23,
            minute: 43,
            second: 2,
        }
    }

    #[test]
    fn single_write_matches_the_specified_geometry() {
        let command = Command::write(Register(326), 1).expect("326 is writable");
        let frame = command.to_frame(SERIAL).expect("build");
        let wire = frame.to_wire();
        assert_eq!(wire.len(), 44, "a single-register write is 44 octets");
        assert_eq!(frame.header().length, 36);
        assert_eq!(frame.body(), [0x01, 0x46, 0x00, 0x01]);
        assert_eq!(command.message_type(), MessageType::WriteSingleRegister);
    }

    #[test]
    fn range_write_length_follows_the_formula() {
        // Length = 36 + 2N.
        for n in 1..=5u16 {
            let pairs: Vec<_> = (0..n).map(|i| (Register(254 + i), 0)).collect();
            let command = Command::write_range(&pairs).expect("slot registers are writable");
            let frame = command.to_frame(SERIAL).expect("build");
            assert_eq!(frame.header().length, 36 + 2 * n);
            assert_eq!(frame.wire_len(), 44 + 2 * usize::from(n));
        }
    }

    #[test]
    fn range_write_end_register_is_inclusive() {
        let command = Command::write_range(&[(Register(250), 100), (Register(251), 5)]).expect("writable");
        let frame = command.to_frame(SERIAL).expect("build");
        // start 250, end 251, then the two values.
        assert_eq!(frame.body(), [0x00, 0xFA, 0x00, 0xFB, 0x00, 0x64, 0x00, 0x05]);
    }

    #[test]
    fn read_repeats_the_register_number() {
        // The worked example from the specification: reading register 322.
        let frame = Command::read(Register(322)).to_frame(SERIAL).expect("build");
        assert_eq!(frame.body(), [0x01, 0x42, 0x01, 0x42]);
        assert_eq!(frame.wire_len(), 44);
        assert_eq!(frame.header().length, 36);
    }

    #[test]
    fn any_register_may_be_read_but_not_written() {
        // Reading has no side effect, so the allowlist does not apply to it.
        let unknown = Register(321);
        assert!(WritableRegister::lookup(unknown).is_none());
        assert!(matches!(
            Command::write(unknown, 1),
            Err(EncodeError::NotWritable { .. })
        ));
        // ...but reading it is fine and needs no permission.
        let frame = Command::read(unknown).to_frame(SERIAL).expect("build");
        assert_eq!(frame.body(), [0x01, 0x41, 0x01, 0x41]);
    }

    #[test]
    fn unknown_registers_are_not_writable() {
        // 321, 341 and 342 are written by the vendor but have no known meaning. None may be written
        // through the general API, and 341/342 not at all.
        for register in [321, 341, 342, 999] {
            assert!(
                matches!(
                    Command::write(Register(register), 0),
                    Err(EncodeError::NotWritable { .. })
                ),
                "register {register} must not be writable"
            );
        }
    }

    #[test]
    fn default_output_power_emits_the_vendor_composite() {
        let command = Command::set_default_output_power(1000).expect("in range");
        let frame = command.to_frame(SERIAL).expect("build");
        // Range 321..322 with a zero in 321, exactly as the vendor server sends it.
        assert_eq!(frame.body(), [0x01, 0x41, 0x01, 0x42, 0x00, 0x00, 0x03, 0xE8]);
        assert_eq!(frame.wire_len(), 48);
    }

    #[test]
    fn set_picks_the_form_the_vendor_uses() {
        // Every setting but one is a single-register write.
        assert!(matches!(
            Command::set(Register(326), 1).expect("writable"),
            Command::WriteSingle { .. }
        ));

        // `default_output_power` is the exception: the range write the vendor sends, not a bare write to
        // 322 that nothing has been observed to accept.
        match Command::set(Register(322), 1000).expect("writable") {
            Command::WriteRange { start, values } => {
                assert_eq!(start, Register(321));
                assert_eq!(values.first().copied().map(Raw::get), Some(0));
                assert_eq!(values.get(1).copied().map(Raw::get), Some(1000));
            }
            other => panic!("expected the composite range write, got {other:?}"),
        }

        // And it verifies 322 — not 321, which has no meaning to compare against.
        let verify = Command::set(Register(322), 1000)
            .expect("writable")
            .registers_to_verify();
        assert_eq!(verify, vec![(Register(322), Some(Raw(1000)))]);
    }

    #[test]
    fn toggling_power_plus_also_verifies_the_power_it_gates() {
        // Clearing the flag drops a stored 1000 W to 800 W with no write to 322, so a verification that
        // covered only what was written would leave a cached value wrong.
        let verify = Command::set(Register(325), 0).expect("writable").registers_to_verify();
        assert_eq!(
            verify,
            vec![(Register(325), Some(Raw(0))), (Register(322), None)],
            "325 should drag 322 in, with no expected value for it"
        );
    }

    #[test]
    fn out_of_range_values_are_refused_with_the_field_named() {
        let err = Command::write(Register(250), 5).expect_err("upper limit starts at 70");
        let text = err.to_string();
        assert!(text.contains("charge_limit_upper"), "{text}");
        assert!(text.contains("70..=100"), "{text}");

        assert!(matches!(
            Command::write(Register(324), 101),
            Err(EncodeError::OutOfRange { .. })
        ));
        assert!(matches!(
            Command::write(Register(326), 2),
            Err(EncodeError::OutOfRange { .. })
        ));
    }

    #[test]
    fn non_consecutive_range_writes_are_refused() {
        let err = Command::write_range(&[(Register(250), 100), (Register(322), 500)])
            .expect_err("250 and 322 are not adjacent");
        assert!(matches!(err, EncodeError::NotConsecutive { .. }));
        assert!(matches!(Command::write_range(&[]), Err(EncodeError::EmptyRange)));
    }

    #[test]
    fn slot_write_covers_all_five_registers() {
        let config = SlotConfig {
            start_hour: 0,
            start_minute: 0,
            end_hour: 23,
            end_minute: 59,
            work_mode: 0,
            output_power: 50,
            enabled: true,
        };
        let command = Command::set_slot(1, config).expect("slot 1 exists");
        let frame = command.to_frame(SERIAL).expect("build");
        assert_eq!(
            frame.body(),
            [
                0x00, 0xFE, 0x01, 0x02, // 254..258
                0x00, 0x00, // 00:00
                0x17, 0x3B, // 23:59
                0x00, 0x00, // load first
                0x00, 0x32, // 50 W
                0x00, 0x01, // enabled
            ]
        );
        assert_eq!(frame.wire_len(), 54);
    }

    #[test]
    fn a_slot_register_can_be_written_on_its_own() {
        // Verified on the device: writing slot1_output_power alone with 0x06 is accepted and takes
        // effect in about a second. This is the cheap path for an output-following control loop, which
        // would otherwise have to restate the window, mode and enabled flag every update.
        let command = Command::write(Register(257), 100).expect("slot output power is writable");
        let frame = command.to_frame(SERIAL).expect("build");
        assert_eq!(command.message_type(), MessageType::WriteSingleRegister);
        assert_eq!(frame.body(), [0x01, 0x01, 0x00, 0x64]);
        assert_eq!(frame.wire_len(), 44);

        // Every field of every slot is individually addressable, with its own domain.
        for slot in 1..=9u16 {
            let registers = slot_registers(slot).expect("slot exists");
            for register in registers {
                assert!(
                    WritableRegister::lookup(register).is_some(),
                    "slot {slot} register {register} should be writable"
                );
            }
        }
    }

    #[test]
    fn setting_slot_power_is_a_single_register_write() {
        // The operation a bridge performs most: one four-octet body, no restating of the window.
        let command = Command::set_slot_output_power(1, 250).expect("slot 1 exists");
        let frame = command.to_frame(SERIAL).expect("build");
        assert_eq!(command.message_type(), MessageType::WriteSingleRegister);
        assert_eq!(frame.wire_len(), 44);
        // Register 257 = 254 + 3, and 250 W.
        assert_eq!(frame.body(), [0x01, 0x01, 0x00, 0xFA]);
        // Not acknowledged, so the caller has to read back.
        assert!(!command.is_acknowledged());
    }

    #[test]
    fn slot_power_addresses_the_right_register_for_every_slot() {
        for slot in 1..=9u16 {
            let command = Command::set_slot_output_power(slot, 100).expect("slot exists");
            let expected = SlotField::OutputPower.register(slot).expect("slot exists").number();
            match command {
                Command::WriteSingle { register, value } => {
                    assert_eq!(register.register().number(), expected, "slot {slot}");
                    assert_eq!(register.name(), format!("slot{slot}_output_power"), "slot {slot}");
                    assert_eq!(value.get(), 100);
                }
                _ => panic!("slot {slot}: expected a single-register write"),
            }
        }
    }

    #[test]
    fn slot_helpers_reject_a_nonexistent_slot() {
        for slot in [0, 10, u16::MAX] {
            assert!(matches!(
                Command::set_slot_output_power(slot, 100),
                Err(EncodeError::NoSuchSlot { .. })
            ));
            assert!(matches!(
                Command::set_slot_enabled(slot, true),
                Err(EncodeError::NoSuchSlot { .. })
            ));
        }
    }

    #[test]
    fn slot_power_is_validated_against_the_register_domain() {
        assert!(matches!(
            Command::set_slot_output_power(1, 1001),
            Err(EncodeError::OutOfRange { .. })
        ));
        assert!(Command::set_slot_output_power(1, 1000).is_ok());
        assert!(Command::set_slot_output_power(1, 0).is_ok());
    }

    #[test]
    fn slot_enabled_toggles_the_flag_register() {
        let on = Command::set_slot_enabled(2, true).expect("slot 2");
        let off = Command::set_slot_enabled(2, false).expect("slot 2");
        // Slot 2 starts at 259, so its enabled flag is 263.
        assert_eq!(on.to_frame(SERIAL).expect("build").body(), [0x01, 0x07, 0x00, 0x01]);
        assert_eq!(off.to_frame(SERIAL).expect("build").body(), [0x01, 0x07, 0x00, 0x00]);
    }

    #[test]
    fn slot_fields_map_to_their_offsets() {
        assert_eq!(SlotField::StartTime.offset(), 0);
        assert_eq!(SlotField::Enabled.offset(), 4);
        assert_eq!(SlotField::OutputPower.register(1).map(Register::number), Some(257));
        assert_eq!(SlotField::WorkMode.register(9).map(Register::number), Some(296));
        assert!(SlotField::StartTime.register(0).is_none());
    }

    #[test]
    fn slot_field_names_follow_the_register_arithmetic() {
        let power = WritableRegister::lookup(Register(257)).expect("257");
        assert_eq!(power.name(), "slot1_output_power");
        // 254 + 5n with n counted from 0, so slot 3 starts at 264 and its mode is 266.
        assert_eq!(slot_registers(3).map(|r| r[0].number()), Some(264));
        let mode = WritableRegister::lookup(Register(266)).expect("266");
        assert_eq!(mode.name(), "slot3_work_mode");
        // And the neighbouring slot's fields do not bleed into it.
        let next = WritableRegister::lookup(Register(270)).expect("270");
        assert_eq!(next.name(), "slot4_end_time");
    }

    #[test]
    fn slot_numbering_starts_at_one_and_stops_at_nine() {
        assert_eq!(
            slot_registers(1).map(|r| r.map(Register::number)),
            Some([254, 255, 256, 257, 258])
        );
        assert_eq!(
            slot_registers(9).map(|r| r.map(Register::number)),
            Some([294, 295, 296, 297, 298])
        );
        assert!(slot_registers(0).is_none());
        assert!(slot_registers(10).is_none());

        let config = SlotConfig {
            start_hour: 0,
            start_minute: 0,
            end_hour: 1,
            end_minute: 0,
            work_mode: 0,
            output_power: 0,
            enabled: false,
        };
        assert!(matches!(
            Command::set_slot(10, config),
            Err(EncodeError::NoSuchSlot { .. })
        ));
    }

    #[test]
    fn slot_fields_are_validated_individually() {
        let base = SlotConfig {
            start_hour: 0,
            start_minute: 0,
            end_hour: 23,
            end_minute: 59,
            work_mode: 0,
            output_power: 0,
            enabled: false,
        };
        // Work mode has three values.
        assert!(matches!(
            Command::set_slot(1, SlotConfig { work_mode: 3, ..base }),
            Err(EncodeError::OutOfRange { .. })
        ));
        // An impossible clock time is caught by the TimeOfDay domain.
        assert!(matches!(
            Command::set_slot(1, SlotConfig { end_minute: 77, ..base }),
            Err(EncodeError::OutOfRange { .. })
        ));
        assert!(matches!(
            Command::set_slot(1, SlotConfig { end_hour: 25, ..base }),
            Err(EncodeError::OutOfRange { .. })
        ));
    }

    #[test]
    fn time_push_geometry_and_text() {
        let command = Command::time_push(stamp()).expect("plausible");
        let frame = command.to_frame(SERIAL).expect("build");
        assert_eq!(frame.wire_len(), 67);
        assert_eq!(frame.header().length, 59);
        assert_eq!(frame.message_type(), MessageType::TimePush);
        assert_eq!(frame.header().address, 0xFE);

        let body = frame.body();
        assert_eq!(body.get(..8), Some(super::TIME_PUSH_PREFIX.as_slice()));
        assert_eq!(
            body.get(8..).map(String::from_utf8_lossy).as_deref(),
            Some("2026-08-06 23:43:02")
        );
        // The trailing pair of the prefix is the string length.
        assert_eq!(usize::from(super::TIME_PUSH_PREFIX[7]), body.len() - 8);
    }

    #[test]
    fn implausible_times_are_refused() {
        let bad = Timestamp { month: 13, ..stamp() };
        assert!(matches!(
            Command::time_push(bad),
            Err(EncodeError::ImplausibleTimestamp { .. })
        ));
    }

    #[test]
    fn acknowledgement_expectations_are_explicit() {
        // A range write is echoed; a single-register write is not. Both still need a read-back,
        // because neither acknowledgement carries a value.
        let range = Command::write_range(&[(Register(250), 100), (Register(251), 5)]).expect("ok");
        let single = Command::write(Register(326), 1).expect("ok");
        assert!(range.is_acknowledged());
        assert!(!single.is_acknowledged());
    }

    #[test]
    fn commands_survive_a_wire_round_trip() {
        let commands = [
            Command::write(Register(326), 1).expect("ok"),
            Command::write_range(&[(Register(250), 100), (Register(251), 5)]).expect("ok"),
            Command::set_default_output_power(800).expect("ok"),
            Command::read(Register(322)),
            Command::time_push(stamp()).expect("ok"),
        ];
        for command in &commands {
            let frame = command.to_frame(SERIAL).expect("build");
            let wire = frame.to_wire();
            let parsed = crate::growatt::v7::frame::Frame::parse(&wire).expect("parse back");
            assert_eq!(parsed, frame, "{command:?} did not survive a round trip");
            assert_eq!(parsed.message_type(), command.message_type());
        }
    }
}
