//! Vendor-neutral data model.
//!
//! Nothing here knows about Growatt, about MQTT or about Home Assistant. These are the types a
//! decoded reading is expressed in, and they are the boundary a second protocol — another Growatt
//! generation, or another vendor entirely — would implement against.
//!
//! The register newtypes live here rather than in the protocol module for the same reason: "16-bit
//! register number" and "16-bit raw value" are concepts any register-based device shares, and the
//! whole point of separating them is that they must never be confused.

use core::fmt;

/// A register number, in whichever address space the context implies.
///
/// A register number is meaningless without its space — input, holding or config for the protocol
/// implemented here. The space is carried by the surrounding type, not by this newtype, because a
/// number that has escaped its space is already a bug.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Register(pub u16);

/// A raw 16-bit register value, exactly as it appeared on the wire, before scaling.
///
/// Kept distinct from a scaled value so that scaling can only be applied once. Applying it twice, or
/// forgetting to, both yield plausible numbers.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Raw(pub u16);

impl Register {
    /// The register's number.
    pub const fn number(self) -> u16 {
        self.0
    }
}

impl Raw {
    /// The unscaled value.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hex-encodes a byte slice when it is formatted, and not before.
///
/// A newtype with a [`fmt::Display`] impl rather than a function returning `String`, because the
/// difference matters here: passed as `%Hex(frame)` inside a `trace!`, no encoding happens at all unless
/// the level is enabled. Frames are 585 octets and arrive every five seconds, so eagerly formatting a
/// dump that is usually discarded is not free.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Hex<'a>(pub &'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A wall-clock time, as a device reports it or as a server sends one.
///
/// Deliberately not a date-time type from a calendar crate. On the way in these are six octets from an
/// untrusted device that must survive being nonsensical without a conversion failing; on the way out the
/// only requirement is to render the fields a protocol asks for. Neither needs arithmetic on calendars.
///
/// Vendor-neutral on purpose: the clock that feeds a server time push should not have to depend on a
/// particular protocol generation's decoder to express what time it is.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Timestamp {
    /// Full year.
    pub year: u16,
    /// Month, 1–12.
    pub month: u8,
    /// Day of month.
    pub day: u8,
    /// Hour.
    pub hour: u8,
    /// Minute.
    pub minute: u8,
    /// Second.
    pub second: u8,
}

impl Timestamp {
    /// Whether the values form a plausible calendar time.
    ///
    /// A frame that fails this is still decoded — the device sends an all-zero timestamp occasionally,
    /// and rejecting the frame over it would discard good telemetry.
    pub const fn is_plausible(self) -> bool {
        self.month >= 1
            && self.month <= 12
            && self.day >= 1
            && self.day <= 31
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// Seconds by which `self` is ahead of `other`, treating both as the same timezone.
    ///
    /// Deliberately crude: a calendar-correct difference would need a date library, and the only question
    /// asked of it is "are these roughly the same moment". `None` when either side is implausible, or
    /// when the two fall on different dates — in which case the answer is "very far apart" rather than a
    /// number worth computing.
    pub const fn skew_from(self, other: Self) -> Option<i64> {
        if !self.is_plausible() || !other.is_plausible() {
            return None;
        }
        if self.year != other.year || self.month != other.month || self.day != other.day {
            return None;
        }
        Some(self.seconds_into_day().saturating_sub(other.seconds_into_day()))
    }

    /// Seconds elapsed since midnight.
    const fn seconds_into_day(self) -> i64 {
        (self.hour as i64)
            .saturating_mul(3600)
            .saturating_add((self.minute as i64).saturating_mul(60))
            .saturating_add(self.second as i64)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } = *self;
        write!(f, "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
    }
}

/// How well a field's meaning is established.
///
/// Carried through from decoding to publication on purpose. A field whose meaning is inferred should
/// not silently become the basis of an automation, and the consumer cannot make that judgement
/// unless the decoder tells it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    /// Seen on the wire; the interpretation is a guess.
    Inferred,
    /// Read out of decompiled vendor software, and not seen on the wire.
    ///
    /// Stronger than a guess and weaker than an observation: the device has not been seen to behave this
    /// way, but the software that drives it says so. Such a meaning is only as good as the decompilation,
    /// and may not hold on another firmware release.
    Vendor,
    /// Seen on the wire and self-consistent, but not checked against an independent source.
    Observed,
    /// Confirmed against an independent reference, or by changing it and watching the result.
    Verified,
}

impl Confidence {
    /// The marker used in the specification, and in anything this publishes.
    ///
    /// Deliberately the same words the documentation uses, so a consumer reading `inferred` from the
    /// API and `[I]` in the specification is reading one claim rather than two vocabularies. `vendor-app`
    /// is the word the Bluetooth client already publishes for the same thing.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inferred => "inferred",
            Self::Vendor => "vendor-app",
            Self::Observed => "observed",
            Self::Verified => "verified",
        }
    }
}

/// The physical unit of a scaled value.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unit {
    /// No unit: a count, an index, a flag.
    None,
    /// Watts.
    Watt,
    /// Percent.
    Percent,
    /// Degrees Celsius.
    Celsius,
    /// Volts.
    Volt,
    /// Amperes.
    Ampere,
    /// Kilowatt-hours.
    KilowattHour,
    /// Seconds.
    Second,
}

impl Unit {
    /// The unit's conventional symbol, or an empty string for [`Unit::None`].
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Watt => "W",
            Self::Percent => "%",
            Self::Celsius => "°C",
            Self::Volt => "V",
            Self::Ampere => "A",
            Self::KilowattHour => "kWh",
            Self::Second => "s",
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol())
    }
}

/// A linear transform from a raw register value to a physical quantity.
///
/// # The order is normative
///
/// `value = raw × multiplier + delta`. Multiply **then** add. Reversing it yields plausible-looking
/// but wrong values — during the protocol work this mistake read a battery temperature as 289.79 °C
/// instead of 44.0, and it was caught only because the number was absurd. [`Scaling::apply`] is the
/// only place this arithmetic happens.
/// # Two ways of being signed
///
/// The register block carries negative quantities in two different encodings, and they are
/// distinguishable: a value of −131 reads `29869` under the offset encoding — a negative [`Self::delta`] —
/// and `65405` under two's complement, which is [`RawEncoding`] applied to the raw bits before the
/// transform above.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Scaling {
    /// Multiplied first.
    pub multiplier: f64,
    /// Added second. Negative for the offset-encoded signed quantities.
    pub delta: f64,
    /// How to read the raw 16 bits before multiplying.
    pub encoding: RawEncoding,
}

/// How a raw register value's bits map to a number, before any scaling.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RawEncoding {
    /// The 16 bits are a magnitude in `0..=65535`.
    Unsigned,
    /// The 16 bits are two's complement, so the top bit is a sign: `65405` is −131.
    TwosComplement,
}

impl Scaling {
    /// Identity: the raw value is already the quantity.
    pub const UNIT: Self = Self {
        multiplier: 1.0,
        delta: 0.0,
        encoding: RawEncoding::Unsigned,
    };

    /// The offset encoding used for signed quantities: 29 950 means −50.
    pub const SIGNED: Self = Self {
        multiplier: 1.0,
        delta: -30_000.0,
        encoding: RawEncoding::Unsigned,
    };

    /// Two's complement, unscaled: `65405` means −131.
    pub const TWOS_COMPLEMENT: Self = Self {
        multiplier: 1.0,
        delta: 0.0,
        encoding: RawEncoding::TwosComplement,
    };

    /// Tenths, unsigned.
    pub const TENTHS: Self = Self {
        multiplier: 0.1,
        delta: 0.0,
        encoding: RawEncoding::Unsigned,
    };

    /// Hundredths, unsigned.
    pub const HUNDREDTHS: Self = Self {
        multiplier: 0.01,
        delta: 0.0,
        encoding: RawEncoding::Unsigned,
    };

    /// Thousandths, unsigned.
    pub const THOUSANDTHS: Self = Self {
        multiplier: 0.001,
        delta: 0.0,
        encoding: RawEncoding::Unsigned,
    };

    /// Temperature in tenths of a Kelvin above absolute zero.
    pub const KELVIN_TENTHS: Self = Self {
        multiplier: 0.1,
        delta: -273.1,
        encoding: RawEncoding::Unsigned,
    };

    /// A scaling with an explicit multiplier and delta, reading the raw value as unsigned.
    pub const fn new(multiplier: f64, delta: f64) -> Self {
        Self {
            multiplier,
            delta,
            encoding: RawEncoding::Unsigned,
        }
    }

    /// Apply the transform. Multiply, then add.
    ///
    /// `f64::from` is lossless for both `u16` and `i16`, so no precision is lost before the multiply.
    /// `mul_add` is the same `raw × multiplier + delta` with a single rounding instead of two.
    pub fn apply(self, raw: Raw) -> f64 {
        let widened = match self.encoding {
            RawEncoding::Unsigned => f64::from(raw.get()),
            RawEncoding::TwosComplement => f64::from(raw.get().cast_signed()),
        };
        widened.mul_add(self.multiplier, self.delta)
    }

    /// The same, for a raw value that spans two registers.
    ///
    /// Separate from [`Self::apply`] because [`Raw`] is a single register by definition, and widening it
    /// would make every setting in the holding map claim a range it does not have.
    ///
    /// Two's complement is not honoured here: no 32-bit quantity in the register map uses it, and a
    /// 16-bit sign bit means nothing in the middle of a wider value.
    pub fn apply_u32(self, raw: u32) -> f64 {
        f64::from(raw).mul_add(self.multiplier, self.delta)
    }

    /// Whether this scaling can produce a negative quantity, by either encoding.
    pub fn is_signed(self) -> bool {
        self.delta < 0.0 || self.encoding == RawEncoding::TwosComplement
    }
}

/// A decoded value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// An integer count or identifier, unscaled.
    Int(u16),
    /// A scaled physical quantity.
    Float(f64),
    /// An enumerated state, with its label where one is known.
    Enum {
        /// The raw index.
        raw: u16,
        /// The label, or `None` for an index outside the known set.
        label: Option<&'static str>,
    },
    /// ASCII text spanning consecutive registers.
    Text(String),
    /// A time of day, encoded `HH << 8 | MM`.
    TimeOfDay {
        /// Hour, 0–23.
        hour: u8,
        /// Minute, 0–59.
        minute: u8,
    },
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(v) => write!(f, "{v}"),
            // Three decimals, then trimmed. Every scaling in the register map is a power of ten down to
            // thousandths, so three digits is lossless here — and the default `{}` is not: `30999 × 0.1 −
            // 3000` renders as `99.90000000000018`, which is arithmetically true and useless to read.
            Self::Float(v) => {
                let rendered = format!("{v:.3}");
                let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
                f.write_str(trimmed)
            }
            Self::Enum { raw, label } => match label {
                Some(name) => write!(f, "{name}"),
                None => write!(f, "{raw}"),
            },
            Self::Text(s) => f.write_str(s),
            Self::TimeOfDay { hour, minute } => write!(f, "{hour:02}:{minute:02}"),
        }
    }
}

/// One decoded field: what it is, what it read, and how much to trust it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Which register it came from.
    pub register: Register,
    /// Its name. Stable across versions of this crate; it appears in published topics.
    pub name: &'static str,
    /// The value as it appeared on the wire, retained for diagnostics.
    pub raw: Raw,
    /// The decoded value.
    pub value: Value,
    /// The unit of [`Reading::value`] when it is a [`Value::Float`].
    pub unit: Unit,
    /// How well the field's meaning is established.
    pub confidence: Confidence,
}

impl Reading {
    /// Whether this field's meaning is unknown, i.e. its name is a placeholder.
    ///
    /// These are decoded and kept for investigation but should not be published as if they meant
    /// something: a value nobody can interpret is noise in a dashboard.
    pub fn is_unknown(&self) -> bool {
        self.name.starts_with("unknown_")
    }

    /// The value as a number, for any variant that has a sensible numeric form.
    pub fn as_f64(&self) -> Option<f64> {
        match self.value {
            Value::Float(v) => Some(v),
            Value::Int(v) | Value::Enum { raw: v, .. } => Some(f64::from(v)),
            Value::Text(_) | Value::TimeOfDay { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Raw, Reading, Register, Scaling, Unit, Value};
    use crate::model::Confidence;

    #[test]
    fn scaling_multiplies_before_adding() {
        // The real case that caught this: register 30, battery temperature, raw 3091.
        // Correct order gives 36.0; the inverted order gives 281.79.
        let scaled = Scaling::KELVIN_TENTHS.apply(Raw(3091));
        assert!((scaled - 36.0).abs() < 1e-9, "got {scaled}");

        let inverted = (f64::from(3091u16) + -273.1) * 0.1;
        assert!(
            (inverted - 36.0).abs() > 1.0,
            "the inverted order should be obviously wrong, got {inverted}"
        );
    }

    #[test]
    fn signed_encoding_round_trips() {
        assert!((Scaling::SIGNED.apply(Raw(29_950)) - -50.0).abs() < 1e-9);
        assert!((Scaling::SIGNED.apply(Raw(30_000)) - 0.0).abs() < 1e-9);
        assert!((Scaling::SIGNED.apply(Raw(30_100)) - 100.0).abs() < 1e-9);
        assert!(Scaling::SIGNED.is_signed());
        assert!(!Scaling::UNIT.is_signed());
    }

    #[test]
    fn twos_complement_reads_the_sign_bit() {
        assert!((Scaling::TWOS_COMPLEMENT.apply(Raw(65_405)) - -131.0).abs() < 1e-9);
        assert!((Scaling::TWOS_COMPLEMENT.apply(Raw(0)) - 0.0).abs() < 1e-9);
        assert!((Scaling::TWOS_COMPLEMENT.apply(Raw(580)) - 580.0).abs() < 1e-9);
        assert!(Scaling::TWOS_COMPLEMENT.is_signed());
    }

    #[test]
    fn the_two_signed_encodings_disagree_on_the_same_bits() {
        // Which is why one can be told from the other: a household load of -131 W arrives as 65405, and
        // reading it under the offset encoding — or as unsigned — gives a number nothing would mistake
        // for correct.
        let raw = Raw(65_405);
        assert!((Scaling::TWOS_COMPLEMENT.apply(raw) - -131.0).abs() < 1e-9);
        assert!((Scaling::SIGNED.apply(raw) - 35_405.0).abs() < 1e-9);
        assert!((Scaling::UNIT.apply(raw) - 65_405.0).abs() < 1e-9);
    }

    #[test]
    fn unit_symbols() {
        assert_eq!(Unit::Watt.symbol(), "W");
        assert_eq!(Unit::None.symbol(), "");
        assert_eq!(Unit::Celsius.to_string(), "°C");
    }

    #[test]
    fn unknown_fields_are_recognisable() {
        let reading = |name| Reading {
            register: Register(110),
            name,
            raw: Raw(30_000),
            value: Value::Float(0.0),
            unit: Unit::None,
            confidence: Confidence::Inferred,
        };
        assert!(reading("unknown_110").is_unknown());
        assert!(!reading("ac_power").is_unknown());
    }

    #[test]
    fn time_of_day_formats_with_leading_zeros() {
        let value = Value::TimeOfDay { hour: 7, minute: 5 };
        assert_eq!(value.to_string(), "07:05");
    }

    #[test]
    fn floats_render_without_binary_noise() {
        // The exact value register 116 produces: 30 999 × 0.1 − 3000. `{}` renders it 99.90000000000018.
        let hires = Scaling::new(0.1, -3000.0).apply(Raw(30_999));
        assert_eq!(Value::Float(hires).to_string(), "99.9");

        // A whole number keeps no decimal point, and thousandths — the finest scaling in the map, used for
        // cell voltages — survive.
        assert_eq!(Value::Float(99.0).to_string(), "99");
        assert_eq!(Value::Float(3.271).to_string(), "3.271");
        assert_eq!(Value::Float(-49.0).to_string(), "-49");
        assert_eq!(Value::Float(30.4).to_string(), "30.4");
    }

    #[test]
    fn hex_pads_each_octet_to_two_digits() {
        use super::Hex;
        assert_eq!(Hex(&[0x00, 0x0F, 0xA5, 0xFF]).to_string(), "000fa5ff");
        assert_eq!(Hex(&[]).to_string(), "");
    }

    #[test]
    fn timestamps_report_their_own_plausibility_and_skew() {
        let base = super::Timestamp {
            year: 2026,
            month: 8,
            day: 8,
            hour: 12,
            minute: 0,
            second: 0,
        };
        assert!(base.is_plausible());
        assert_eq!(base.to_string(), "2026-08-08 12:00:00");

        let later = super::Timestamp { second: 30, ..base };
        assert_eq!(later.skew_from(base), Some(30));
        assert_eq!(base.skew_from(later), Some(-30));
    }
}
