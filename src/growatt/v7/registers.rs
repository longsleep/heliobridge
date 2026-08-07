//! Register maps for generation 7.
//!
//! Three distinct address spaces exist; a register number is meaningful only with its space. This
//! module currently covers the **input** space — telemetry, read-only, carried in `0x04` frames.
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
