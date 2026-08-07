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

/// How well a field's meaning is established.
///
/// Carried through from decoding to publication on purpose. A field whose meaning is inferred should
/// not silently become the basis of an automation, and the consumer cannot make that judgement
/// unless the decoder tells it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    /// Seen on the wire; the interpretation is a guess.
    Inferred,
    /// Seen on the wire and self-consistent, but not checked against an independent source.
    Observed,
    /// Confirmed against an independent reference, or by changing it and watching the result.
    Verified,
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
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Scaling {
    /// Multiplied first.
    pub multiplier: f64,
    /// Added second. Negative for the offset-encoded signed quantities.
    pub delta: f64,
}

impl Scaling {
    /// Identity: the raw value is already the quantity.
    pub const UNIT: Self = Self {
        multiplier: 1.0,
        delta: 0.0,
    };

    /// The offset encoding used for signed quantities: 29 950 means −50.
    pub const SIGNED: Self = Self {
        multiplier: 1.0,
        delta: -30_000.0,
    };

    /// Tenths, unsigned.
    pub const TENTHS: Self = Self {
        multiplier: 0.1,
        delta: 0.0,
    };

    /// Hundredths, unsigned.
    pub const HUNDREDTHS: Self = Self {
        multiplier: 0.01,
        delta: 0.0,
    };

    /// Thousandths, unsigned.
    pub const THOUSANDTHS: Self = Self {
        multiplier: 0.001,
        delta: 0.0,
    };

    /// Temperature in tenths of a Kelvin above absolute zero.
    pub const KELVIN_TENTHS: Self = Self {
        multiplier: 0.1,
        delta: -273.1,
    };

    /// A scaling with an explicit multiplier and delta.
    pub const fn new(multiplier: f64, delta: f64) -> Self {
        Self { multiplier, delta }
    }

    /// Apply the transform. Multiply, then add.
    ///
    /// `f64::from` is lossless for `u16`, so no precision is lost before the multiply. `mul_add` is
    /// the same `raw × multiplier + delta` with a single rounding instead of two.
    pub fn apply(self, raw: Raw) -> f64 {
        f64::from(raw.get()).mul_add(self.multiplier, self.delta)
    }

    /// Whether this scaling encodes a signed quantity via a negative delta.
    pub fn is_signed(self) -> bool {
        self.delta < 0.0
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
            Self::Float(v) => write!(f, "{v}"),
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
}
