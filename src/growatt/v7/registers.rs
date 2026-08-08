//! Register maps for generation 7.
//!
//! Three distinct address spaces exist; a register number is meaningful only with its space. This
//! module covers two of them:
//!
//! - **input** — telemetry, read-only, carried in `0x04` frames and located by frame offset.
//! - **holding** — settings, read/write, written by register number via `0x06` and `0x10`.
//!
//! The config space, reported as TLV tags in the `0x19` identity frame, is not modelled yet.
//!
//! # Locating an input register
//!
//! ```text
//! absolute_frame_offset = 0x4F + 2 × register_number
//! ```
//!
//! Offsets are absolute from the start of the frame, which is why [`super::Frame`] keeps the whole
//! deobfuscated frame rather than just the body.
//!
//! # Why the table is a `const` slice
//!
//! It is validated at compile time — see the `const` assertion below, which rejects a table that is
//! unsorted or has a duplicate register number. A generated table from an external data file was the
//! original plan, and remains a reasonable step once the map stops changing; the property that
//! matters is that a malformed table cannot compile, and a `const` slice with a `const` check already
//! has it, without a build script or a parser.

use crate::model::{Confidence, Raw, Register, Scaling, Unit, Value};

/// Base offset of the input register block within a telemetry frame.
pub const INPUT_BASE_OFFSET: usize = 0x4F;

/// What kind of quantity a register carries.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    /// An unscaled integer: a count or an identifier.
    Int,
    /// A scaled physical quantity.
    Float,
    /// An index into a set of labels.
    Enum(&'static [&'static str]),
    /// ASCII text spanning `registers` consecutive registers.
    Text {
        /// How many registers the text occupies. Two octets each.
        registers: u16,
    },
}

/// Labels for `work_mode`, input register 8 and slot register `+2`.
pub const WORK_MODE_LABELS: &[&str] = &["load_first", "battery_first", "smart_self_use"];

/// Labels for `battery_charge_status`, input register 10.
pub const BATTERY_STATUS_LABELS: &[&str] = &["idle", "charging", "discharging"];

/// One entry in the input register map.
#[derive(Debug, Copy, Clone)]
pub struct InputRegister {
    /// The register number.
    pub register: Register,
    /// Field name. This appears in published topics, so it is part of the public interface.
    pub name: &'static str,
    /// How to interpret the raw value.
    pub kind: Kind,
    /// Unit of the scaled value.
    pub unit: Unit,
    /// How to scale the raw value.
    pub scaling: Scaling,
    /// How well the meaning is established.
    pub confidence: Confidence,
}

impl InputRegister {
    /// A scaled physical quantity.
    pub const fn float(
        register: u16,
        name: &'static str,
        unit: Unit,
        scaling: Scaling,
        confidence: Confidence,
    ) -> Self {
        Self {
            register: Register(register),
            name,
            kind: Kind::Float,
            unit,
            scaling,
            confidence,
        }
    }

    /// An unscaled integer: a count or an identifier.
    pub const fn int(register: u16, name: &'static str, confidence: Confidence) -> Self {
        Self {
            register: Register(register),
            name,
            kind: Kind::Int,
            unit: Unit::None,
            scaling: Scaling::UNIT,
            confidence,
        }
    }

    /// An index into a set of labels.
    pub const fn enumerated(
        register: u16,
        name: &'static str,
        labels: &'static [&'static str],
        confidence: Confidence,
    ) -> Self {
        Self {
            register: Register(register),
            name,
            kind: Kind::Enum(labels),
            unit: Unit::None,
            scaling: Scaling::UNIT,
            confidence,
        }
    }

    /// ASCII text spanning `registers` consecutive registers, two octets each.
    pub const fn text(register: u16, name: &'static str, registers: u16, confidence: Confidence) -> Self {
        Self {
            register: Register(register),
            name,
            kind: Kind::Text { registers },
            unit: Unit::None,
            scaling: Scaling::UNIT,
            confidence,
        }
    }

    /// The absolute frame offset a given register is read from.
    ///
    /// Computed in `usize`: `0x4F + 2 × register` overflows `u16` for register numbers above 32 744,
    /// which is inside the representable range of the field.
    pub const fn offset_of(register: Register) -> usize {
        let index = register.number() as usize;
        INPUT_BASE_OFFSET.saturating_add(index.saturating_mul(2))
    }

    /// Look up an entry by register number.
    ///
    /// Binary search, which the compile-time sortedness check makes safe to rely on.
    pub fn lookup(register: Register) -> Option<&'static Self> {
        let index = INPUT_REGISTERS
            .binary_search_by_key(&register, |entry| entry.register)
            .ok()?;
        INPUT_REGISTERS.get(index)
    }

    /// The absolute frame offset this entry is read from.
    pub const fn offset(&self) -> usize {
        Self::offset_of(self.register)
    }

    /// Whether the field's meaning is unknown and its name a placeholder.
    pub fn is_unknown(&self) -> bool {
        self.name.starts_with("unknown_")
    }

    /// Decode a raw value according to this entry.
    pub fn decode(&self, raw: Raw) -> Value {
        match self.kind {
            Kind::Float => Value::Float(self.scaling.apply(raw)),
            Kind::Enum(labels) => Value::Enum {
                raw: raw.get(),
                label: labels.get(usize::from(raw.get())).copied(),
            },
            // Text spans several registers, so a single raw value cannot produce it. Sharing the
            // `Int` body is deliberate rather than an oversight: the decoder reads text octets
            // directly, and this arm exists only to keep the match exhaustive.
            Kind::Int | Kind::Text { .. } => Value::Int(raw.get()),
        }
    }
}

/// Every documented input register, ordered by register number.
///
/// `unknown_*` entries are deliberately included. They decode, they are logged, and they are how the
/// next field gets identified — but a consumer should not present them as if they meant something.
///
/// `Entry` is a local alias for [`InputRegister`], so that each row of the table fits on one line and
/// the table stays scannable against the specification's appendix.
pub const INPUT_REGISTERS: &[InputRegister] = {
    use Confidence::{Inferred, Observed, Verified};
    use InputRegister as Entry;
    use Unit::{Ampere, Celsius, KilowattHour, None as NoUnit, Percent, Volt, Watt};

    &[
        Entry::float(5, "ac_power", Watt, Scaling::SIGNED, Verified),
        Entry::float(7, "pv_power_total", Watt, Scaling::UNIT, Verified),
        Entry::enumerated(8, "work_mode", WORK_MODE_LABELS, Verified),
        Entry::enumerated(10, "battery_charge_status", BATTERY_STATUS_LABELS, Observed),
        Entry::float(11, "battery_charge_power", Watt, Scaling::SIGNED, Verified),
        Entry::int(12, "battery_pack_count", Observed),
        Entry::float(13, "battery_soc_total", Percent, Scaling::UNIT, Verified),
        Entry::float(16, "household_load_total", Watt, Scaling::SIGNED, Inferred),
        Entry::float(17, "household_load_excl_groplug", Watt, Scaling::SIGNED, Inferred),
        Entry::text(21, "serial_number_part_1", 2, Observed),
        Entry::text(23, "serial_number_part_2", 2, Observed),
        Entry::text(25, "serial_number_part_3", 2, Observed),
        Entry::text(27, "serial_number_part_4", 2, Observed),
        Entry::float(29, "battery1_soc", Percent, Scaling::UNIT, Verified),
        Entry::float(30, "battery1_temp", Celsius, Scaling::KELVIN_TENTHS, Verified),
        Entry::float(41, "battery2_soc", Percent, Scaling::UNIT, Observed),
        Entry::float(53, "battery3_soc", Percent, Scaling::UNIT, Observed),
        Entry::float(65, "battery4_soc", Percent, Scaling::UNIT, Observed),
        Entry::float(71, "unknown_71", NoUnit, Scaling::UNIT, Inferred),
        Entry::float(72, "energy_today", KilowattHour, Scaling::TENTHS, Verified),
        Entry::float(74, "energy_month", KilowattHour, Scaling::TENTHS, Observed),
        Entry::float(76, "energy_year", KilowattHour, Scaling::TENTHS, Observed),
        Entry::float(78, "energy_total", KilowattHour, Scaling::TENTHS, Verified),
        Entry::float(90, "charge_limit_upper", Percent, Scaling::UNIT, Verified),
        Entry::float(91, "charge_limit_lower", Percent, Scaling::UNIT, Verified),
        Entry::float(92, "pv1_voltage", Volt, Scaling::HUNDREDTHS, Observed),
        Entry::float(93, "pv1_current", Ampere, Scaling::HUNDREDTHS, Observed),
        Entry::float(94, "pv1_temp", Celsius, Scaling::HUNDREDTHS, Observed),
        Entry::float(95, "pv2_voltage", Volt, Scaling::HUNDREDTHS, Observed),
        Entry::float(96, "pv2_current", Ampere, Scaling::HUNDREDTHS, Observed),
        Entry::float(97, "pv2_temp", Celsius, Scaling::HUNDREDTHS, Observed),
        Entry::float(98, "device_temp", Celsius, Scaling::HUNDREDTHS, Observed),
        Entry::float(99, "battery_cell_voltage_max", Volt, Scaling::THOUSANDTHS, Observed),
        Entry::float(100, "battery_cell_voltage_min", Volt, Scaling::THOUSANDTHS, Observed),
        Entry::int(101, "battery_cycles", Observed),
        Entry::float(102, "battery_soh", Percent, Scaling::UNIT, Observed),
        Entry::float(103, "pv3_voltage", Volt, Scaling::HUNDREDTHS, Observed),
        Entry::float(104, "pv3_current", Ampere, Scaling::HUNDREDTHS, Observed),
        Entry::float(105, "pv3_temp", Celsius, Scaling::HUNDREDTHS, Observed),
        Entry::float(106, "pv4_voltage", Volt, Scaling::HUNDREDTHS, Observed),
        Entry::float(107, "pv4_current", Ampere, Scaling::HUNDREDTHS, Observed),
        Entry::float(108, "pv4_temp", Celsius, Scaling::HUNDREDTHS, Observed),
        // 110, 111 and 117 use the signed-power encoding, so they are probably power values.
        Entry::float(110, "unknown_110", NoUnit, Scaling::SIGNED, Inferred),
        Entry::float(111, "unknown_111", NoUnit, Scaling::SIGNED, Inferred),
        Entry::float(112, "unknown_112", NoUnit, Scaling::UNIT, Inferred),
        Entry::float(114, "unknown_114", NoUnit, Scaling::UNIT, Inferred),
        Entry::float(115, "grid_voltage", Volt, Scaling::HUNDREDTHS, Observed),
        Entry::float(116, "ac_power_hires", Watt, Scaling::new(0.1, -3000.0), Verified),
        Entry::float(117, "unknown_117", NoUnit, Scaling::SIGNED, Inferred),
        // Two registers reading 0x0E0C and 0x0E0B, i.e. plain integers rather than the ASCII a
        // version string would be. A pair of component version numbers is the best available
        // reading, and is unconfirmed.
        Entry::int(119, "sw_version_part_1", Inferred),
        Entry::int(120, "sw_version_part_2", Inferred),
    ]
};

// --- compile-time validation ---------------------------------------------------------------------

/// Reject a table that is unsorted or contains a duplicate register number.
///
/// Strictly increasing gives both properties at once, and sortedness is what makes the binary search
/// in [`input_register`] correct. This is the check a generated table would have been written to
/// provide.
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "const context: slice::get is not const, and const overflow is a compile error anyway"
)]
const fn strictly_increasing(table: &[InputRegister]) -> bool {
    let mut i = 1;
    while i < table.len() {
        if table[i - 1].register.0 >= table[i].register.0 {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    strictly_increasing(INPUT_REGISTERS),
    "INPUT_REGISTERS must be sorted by register number with no duplicates"
);

// --- holding registers: settings, read/write ------------------------------------------------------

/// First register of the first schedule slot.
pub const SLOT_BASE: u16 = 254;

/// Registers per schedule slot.
pub const SLOT_STRIDE: u16 = 5;

/// How many schedule slots the device provides.
pub const SLOT_COUNT: u16 = 9;

/// What values a holding register accepts.
///
/// The device silently clamps out-of-range writes rather than rejecting them, so a write that looks
/// successful may not have stored what was asked. Validating here turns a silent clamp into a
/// refusal, which is the more useful outcome for a caller.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Domain {
    /// An inclusive numeric range.
    Range {
        /// Smallest accepted value.
        min: u16,
        /// Largest accepted value.
        max: u16,
    },
    /// A boolean, written as 0 or 1.
    Flag,
    /// A time of day, encoded `HH << 8 | MM`.
    TimeOfDay,
    /// An index into a set of labels.
    Enum(&'static [&'static str]),
}

impl Domain {
    /// Whether a raw value is acceptable.
    pub fn accepts(self, value: u16) -> bool {
        match self {
            Self::Range { min, max } => value >= min && value <= max,
            Self::Flag => value <= 1,
            Self::TimeOfDay => {
                let (hour, minute) = (value >> 8, value & 0xFF);
                hour < 24 && minute < 60
            }
            Self::Enum(labels) => usize::from(value) < labels.len(),
        }
    }

    /// A human-readable description of what is accepted, for error messages.
    pub fn describe(self) -> String {
        match self {
            Self::Range { min, max } => format!("{min}..={max}"),
            Self::Flag => "0 or 1".to_owned(),
            Self::TimeOfDay => "HH<<8|MM with HH<24 and MM<60".to_owned(),
            Self::Enum(labels) => format!("0..={}", labels.len().saturating_sub(1)),
        }
    }
}

/// One entry in the holding register map: a setting that can be read and written.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct HoldingRegister {
    /// The register number, which is the normative identifier.
    pub register: Register,
    /// Field name, following the device's own label where it exposes one.
    pub name: &'static str,
    /// What values are accepted.
    pub domain: Domain,
    /// Unit of the value.
    pub unit: Unit,
    /// How well the meaning is established.
    pub confidence: Confidence,
}

impl HoldingRegister {
    /// A numeric setting with an inclusive range.
    pub const fn range(
        register: u16,
        name: &'static str,
        min: u16,
        max: u16,
        unit: Unit,
        confidence: Confidence,
    ) -> Self {
        Self {
            register: Register(register),
            name,
            domain: Domain::Range { min, max },
            unit,
            confidence,
        }
    }

    /// A boolean setting.
    pub const fn flag(register: u16, name: &'static str, confidence: Confidence) -> Self {
        Self {
            register: Register(register),
            name,
            domain: Domain::Flag,
            unit: Unit::None,
            confidence,
        }
    }

    /// A time-of-day setting.
    pub const fn time(register: u16, name: &'static str, confidence: Confidence) -> Self {
        Self {
            register: Register(register),
            name,
            domain: Domain::TimeOfDay,
            unit: Unit::None,
            confidence,
        }
    }

    /// An enumerated setting.
    pub const fn enumerated(
        register: u16,
        name: &'static str,
        labels: &'static [&'static str],
        confidence: Confidence,
    ) -> Self {
        Self {
            register: Register(register),
            name,
            domain: Domain::Enum(labels),
            unit: Unit::None,
            confidence,
        }
    }

    /// Look up a holding register, including the schedule slots.
    ///
    /// Returns by value rather than by reference because slot entries are computed: nine slots of five
    /// registers is 45 entries that would otherwise have to be written out to be borrowed from.
    pub fn lookup(register: Register) -> Option<Self> {
        if let Some(found) = HOLDING_REGISTERS.iter().find(|entry| entry.register == register) {
            return Some(*found);
        }
        Self::slot_field(register)
    }

    /// The slot entry covering a register, if it falls inside the schedule block.
    fn slot_field(register: Register) -> Option<Self> {
        let offset = register.number().checked_sub(SLOT_BASE)?;
        // Integer division is the intent here: the slot index and the field within it.
        let slot = offset.checked_div(SLOT_STRIDE)?;
        let field = offset.checked_rem(SLOT_STRIDE)?;
        if slot >= SLOT_COUNT {
            return None;
        }
        let names = SLOT_FIELD_NAMES.get(usize::from(slot))?;
        let name = *names.get(usize::from(field))?;
        Some(match field {
            0 | 1 => Self::time(register.number(), name, Confidence::Verified),
            2 => Self::enumerated(register.number(), name, WORK_MODE_LABELS, Confidence::Verified),
            3 => Self::range(register.number(), name, 0, 1000, Unit::Watt, Confidence::Verified),
            _ => Self::flag(register.number(), name, Confidence::Verified),
        })
    }

    /// Decode a raw value according to this entry's domain.
    ///
    /// Mirrors [`InputRegister::decode`], so a setting read back renders the same way a telemetry field
    /// does — a flag as a flag, a slot boundary as a time, a work mode as its label.
    pub fn decode(&self, raw: Raw) -> Value {
        match self.domain {
            Domain::Flag => Value::Int(u16::from(raw.get() != 0)),
            Domain::TimeOfDay => Value::TimeOfDay {
                hour: u8::try_from(raw.get() >> 8).unwrap_or(0),
                minute: u8::try_from(raw.get() & 0xFF).unwrap_or(0),
            },
            Domain::Enum(labels) => Value::Enum {
                raw: raw.get(),
                label: labels.get(usize::from(raw.get())).copied(),
            },
            Domain::Range { .. } => Value::Int(raw.get()),
        }
    }

    /// Every setting worth reading back at startup, for `slots` exposed schedule slots.
    ///
    /// This is the resync set. Switch positions never appear in periodic telemetry, so without a read they
    /// are visible only in the hourly settings snapshot — meaning a server that restarts mid-hour would
    /// otherwise publish nothing for them, or worse, publish a stale guess.
    ///
    /// Ordered with the fixed settings first and the slots after, so the interesting values arrive early if
    /// the sequence is interrupted.
    pub fn resync_set(slots: u16) -> Vec<Self> {
        let mut out: Vec<Self> = HOLDING_REGISTERS.to_vec();
        for slot in 1..=slots.min(SLOT_COUNT) {
            if let Some(entries) = Self::slot(slot) {
                out.extend_from_slice(&entries);
            }
        }
        out
    }

    /// The registers of one schedule slot, `slot` counted from 1.
    pub fn slot(slot: u16) -> Option<[Self; 5]> {
        if slot == 0 || slot > SLOT_COUNT {
            return None;
        }
        let base = SLOT_BASE.checked_add(slot.checked_sub(1)?.checked_mul(SLOT_STRIDE)?)?;
        let mut out = [Self::flag(0, "", Confidence::Inferred); 5];
        for (index, entry) in out.iter_mut().enumerate() {
            let number = base.checked_add(u16::try_from(index).ok()?)?;
            *entry = Self::slot_field(Register(number))?;
        }
        Some(out)
    }
}

/// Field names for each schedule slot.
///
/// Written out rather than composed, because a name has to be `&'static str` to travel in a reading
/// and there is no way to concatenate one at compile time.
const SLOT_FIELD_NAMES: [[&str; 5]; 9] = [
    [
        "slot1_start_time",
        "slot1_end_time",
        "slot1_work_mode",
        "slot1_output_power",
        "slot1_enabled",
    ],
    [
        "slot2_start_time",
        "slot2_end_time",
        "slot2_work_mode",
        "slot2_output_power",
        "slot2_enabled",
    ],
    [
        "slot3_start_time",
        "slot3_end_time",
        "slot3_work_mode",
        "slot3_output_power",
        "slot3_enabled",
    ],
    [
        "slot4_start_time",
        "slot4_end_time",
        "slot4_work_mode",
        "slot4_output_power",
        "slot4_enabled",
    ],
    [
        "slot5_start_time",
        "slot5_end_time",
        "slot5_work_mode",
        "slot5_output_power",
        "slot5_enabled",
    ],
    [
        "slot6_start_time",
        "slot6_end_time",
        "slot6_work_mode",
        "slot6_output_power",
        "slot6_enabled",
    ],
    [
        "slot7_start_time",
        "slot7_end_time",
        "slot7_work_mode",
        "slot7_output_power",
        "slot7_enabled",
    ],
    [
        "slot8_start_time",
        "slot8_end_time",
        "slot8_work_mode",
        "slot8_output_power",
        "slot8_enabled",
    ],
    [
        "slot9_start_time",
        "slot9_end_time",
        "slot9_work_mode",
        "slot9_output_power",
        "slot9_enabled",
    ],
];

/// Writable settings, excluding the schedule slots, which [`HoldingRegister::lookup`] computes.
///
/// **Register 321 is deliberately absent.** Its meaning is unknown, and the vendor server writes `0`
/// to it as part of the range write that carries `default_output_power`. That composite write is
/// reproduced in the encoder as one operation; admitting 321 here would additionally allow a nonzero
/// value, or a write to it on its own, neither of which was ever observed. Registers 341 and 342 are
/// absent for the same reason.
pub const HOLDING_REGISTERS: &[HoldingRegister] = {
    use Confidence::Verified;
    use HoldingRegister as Entry;

    &[
        Entry::range(250, "charge_limit_upper", 70, 100, Unit::Percent, Verified),
        Entry::range(251, "charge_limit_lower", 0, 30, Unit::Percent, Verified),
        Entry::flag(304, "always_on", Verified),
        Entry::flag(305, "ac_output_always_on", Verified),
        // Ceiling is 800 W unless power_plus (325) is set; the device clamps and re-clamps on its own,
        // so the domain is the wider one and the caller must read back.
        Entry::range(322, "default_output_power", 0, 1000, Unit::Watt, Verified),
        Entry::flag(323, "anti_backflow_enabled", Verified),
        Entry::range(324, "anti_backflow_power_percent", 0, 100, Unit::Percent, Verified),
        Entry::flag(325, "power_plus", Verified),
        Entry::flag(326, "grid_power_allowed", Verified),
        Entry::flag(327, "off_grid_mode", Verified),
    ]
};

#[cfg(test)]
mod tests {
    use super::{BATTERY_STATUS_LABELS, INPUT_BASE_OFFSET, INPUT_REGISTERS, InputRegister, Kind, WORK_MODE_LABELS};
    use crate::model::{Confidence, Raw, Register, Unit, Value};

    #[test]
    fn offsets_match_the_specified_formula() {
        assert_eq!(InputRegister::offset_of(Register(0)), INPUT_BASE_OFFSET);
        assert_eq!(InputRegister::offset_of(Register(5)), 0x4F + 10);
        // Register 21 is where the embedded serial starts, at offset 121.
        assert_eq!(InputRegister::offset_of(Register(21)), 121);
        assert_eq!(InputRegister::offset_of(Register(116)), 0x4F + 232);
    }

    #[test]
    fn offset_does_not_overflow_for_any_register() {
        // 0x4F + 2 × 65535 overflows u16, which is why the arithmetic is done in usize.
        assert_eq!(InputRegister::offset_of(Register(u16::MAX)), 0x4F + 131_070);
    }

    #[test]
    fn lookup_finds_entries_and_rejects_gaps() {
        let ac = InputRegister::lookup(Register(5)).expect("register 5 is documented");
        assert_eq!(ac.name, "ac_power");
        assert_eq!(ac.unit, Unit::Watt);
        assert!(ac.scaling.is_signed());

        // 6 and 9 are gaps in the map, not registers with unknown meaning.
        assert!(InputRegister::lookup(Register(6)).is_none());
        assert!(InputRegister::lookup(Register(9)).is_none());
    }

    #[test]
    fn every_name_is_unique() {
        let mut names: Vec<&str> = INPUT_REGISTERS.iter().map(|entry| entry.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate field name in INPUT_REGISTERS");
    }

    #[test]
    fn names_are_snake_case() {
        for entry in INPUT_REGISTERS {
            assert!(
                entry
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{} is not snake_case",
                entry.name
            );
        }
    }

    #[test]
    fn enum_registers_decode_their_labels() {
        let work_mode = InputRegister::lookup(Register(8)).expect("register 8");
        assert_eq!(
            work_mode.decode(Raw(1)),
            Value::Enum {
                raw: 1,
                label: Some("battery_first")
            }
        );
        // An index outside the known set must not invent a label.
        assert_eq!(work_mode.decode(Raw(9)), Value::Enum { raw: 9, label: None });
        assert_eq!(WORK_MODE_LABELS.len(), 3);
        assert_eq!(BATTERY_STATUS_LABELS.len(), 3);
    }

    #[test]
    fn unknown_entries_are_marked_inferred() {
        for entry in INPUT_REGISTERS.iter().filter(|e| e.is_unknown()) {
            assert_eq!(
                entry.confidence,
                Confidence::Inferred,
                "{} is unknown but not marked inferred",
                entry.name
            );
        }
    }

    #[test]
    fn the_unverified_household_registers_are_marked_as_such() {
        // Named after an intent, not a confirmed measurement. Register 16 in particular tracked the
        // output setpoint closely enough to be mistaken for AC output during the protocol work.
        for number in [16, 17] {
            let entry = InputRegister::lookup(Register(number)).expect("documented");
            assert_eq!(entry.confidence, Confidence::Inferred, "register {number}");
        }
    }

    #[test]
    fn the_resync_set_covers_the_settings_plus_the_exposed_slots() {
        use super::HoldingRegister;

        // Ten documented settings, then five registers per exposed slot.
        let one = HoldingRegister::resync_set(1);
        assert_eq!(one.len(), super::HOLDING_REGISTERS.len() + 5);
        let nine = HoldingRegister::resync_set(9);
        assert_eq!(nine.len(), super::HOLDING_REGISTERS.len() + 45);

        // Fixed settings come first, so the interesting values arrive early if the sequence is cut short.
        assert_eq!(one.first().map(|e| e.register), Some(Register(250)));
        assert!(
            one.get(super::HOLDING_REGISTERS.len()).map(|e| e.name) == Some("slot1_start_time"),
            "the slots should follow the fixed settings"
        );

        // Asking for more slots than exist is clamped rather than refused: the caller's number came from
        // configuration, and nine is the honest answer to "give me twenty".
        assert_eq!(HoldingRegister::resync_set(99).len(), nine.len());

        // Register 321 must not appear. It is unknown, and reading is harmless — but the resync set is
        // also what will drive published entities, and an unknown register has nothing to publish.
        assert!(!nine.iter().any(|e| e.register == Register(321)));
    }

    #[test]
    fn holding_registers_decode_their_own_values() {
        use super::HoldingRegister;

        let flag = HoldingRegister::lookup(Register(326)).expect("grid_power_allowed");
        assert_eq!(flag.decode(Raw(1)), Value::Int(1));
        assert_eq!(flag.decode(Raw(0)), Value::Int(0));
        // Anything non-zero is on; the device is not obliged to store exactly 1.
        assert_eq!(flag.decode(Raw(7)), Value::Int(1));

        let start = HoldingRegister::lookup(Register(254)).expect("slot1_start_time");
        assert_eq!(start.decode(Raw(0x173B)), Value::TimeOfDay { hour: 23, minute: 59 });

        let mode = HoldingRegister::lookup(Register(256)).expect("slot1_work_mode");
        assert_eq!(
            mode.decode(Raw(1)),
            Value::Enum {
                raw: 1,
                label: Some("battery_first")
            }
        );

        let power = HoldingRegister::lookup(Register(322)).expect("default_output_power");
        assert_eq!(power.decode(Raw(800)), Value::Int(800));
    }

    #[test]
    fn text_registers_declare_their_span() {
        let part = InputRegister::lookup(Register(21)).expect("register 21");
        assert_eq!(part.kind, Kind::Text { registers: 2 });
        // Four parts of two registers each: the 16-character serial.
        let total: u16 = [21, 23, 25, 27]
            .iter()
            .filter_map(|n| InputRegister::lookup(Register(*n)))
            .filter_map(|e| match e.kind {
                Kind::Text { registers } => Some(registers),
                _ => None,
            })
            .sum();
        assert_eq!(total * 2, 16);
    }
}
