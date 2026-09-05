//! The meter interface of protocol generation 7: the four holding registers that carry a reading.
//!
//! The device regulates against a smart meter's reading of the grid connection. It normally obtains one
//! itself, by polling a meter it has been paired with; it also accepts one written straight into its
//! holding registers, which is what this module is about. That is the whole of the meter interface as far
//! as a replacement server is concerned — a server can supply readings without any metering hardware.
//!
//! Distinct from [`crate::server::meter`], which is an HTTP server *impersonating* a Shelly for the
//! device to poll. This is the register path: no discovery, no polling, the value written directly.
//!
//! # Nothing here refreshes anything
//!
//! A supplied reading expires after [`READING_LIFETIME`], and that expiry is a safety property rather
//! than an inconvenience: it is what stops the device regulating against a figure whose source has gone
//! away. So a reading is a heartbeat, and **whoever holds the measurement sends it** — Home Assistant,
//! from its own power sensors, on its own timer. This program measures nothing, so a refresh loop here
//! would re-assert a number it cannot vouch for, and would defeat the one mechanism that makes a dead
//! publisher safe.
//!
//! The consequence is deliberate: stop writing and the device stops trusting the reading within two
//! minutes, with nothing having to notice or clean up.
//!
//! # Nothing here remembers a reading either
//!
//! There is no record of what was last supplied. The device reports the reading it currently holds in
//! input register 19 (`meter_active_power`), which is the honest state: it falls to zero on expiry by
//! itself, whereas a value remembered here would keep claiming to be in effect after the device had
//! stopped honouring it.
//!
//! # This fabricates a measurement
//!
//! Everything else this program publishes is something the device said. A supplied reading is the
//! opposite: a number of our choosing that the device treats as though it came from an instrument, and
//! acts on without checking it against anything it measures itself. It will discharge a battery to serve
//! a load that is not there.

use core::time::Duration;

use crate::growatt::v7::encode::{Command, EncodeError};
use crate::model::{Raw, Register};

/// First of the four registers carrying a meter reading, `0x135`.
pub const FIRST_REGISTER: Register = Register(309);

/// How many registers a reading occupies.
pub const REGISTER_COUNT: usize = 4;

/// How long the device honours a supplied reading before clearing it.
///
/// Measured: a single write held for about 125 s and was then zeroed with nothing else written. A
/// publisher of readings has to write again inside this, or accept that the device drops the reading —
/// and with it any mode that depends on one.
pub const READING_LIFETIME: Duration = Duration::from_mins(2);

/// Encode a reading as the four registers the device expects.
///
/// The datalogger builds these from `0x135` = **309** after polling a meter, so this reproduces them
/// **`[F]`**:
///
/// | Register | Content |
/// |---|---|
/// | 309 | a 16-bit field of unidentified meaning; the firmware sources it from the meter struct |
/// | 310 | the magnitude when the meter's signed power is **negative** |
/// | 311 | the magnitude when it is **non-negative** |
/// | 312 | a validity flag in the low octet |
///
/// Direction is carried by *which* register holds the magnitude rather than by a sign, and the unused one
/// is zero — so a caller passes a signed figure and the split happens here. An invalid reading writes all
/// four as zero, which is what the firmware itself does for a meter that is not answering, and is
/// therefore how a reading is withdrawn.
///
/// Register 309 is left zero: its meaning is unknown, and writing a guess into a field the consumer may
/// range-check risks having the whole block rejected for the sake of a value nothing needs.
///
/// # Errors
///
/// [`EncodeError`] if the magnitude does not fit a register, which bounds a reading to ±65535 W — far
/// outside anything this equipment could see.
pub fn command(watts: i32, valid: bool) -> Result<Command, EncodeError> {
    let magnitude = u16::try_from(watts.abs()).map_err(|_ignored| EncodeError::OutOfRange {
        name: "supplied meter power",
        register: Register(if watts < 0 { 310 } else { 311 }),
        accepted: "a magnitude of at most 65535 W".to_owned(),
        value: 0,
    })?;
    let (negative, positive) = if watts < 0 { (magnitude, 0) } else { (0, magnitude) };

    Ok(Command::WriteRange {
        start: FIRST_REGISTER,
        values: vec![
            Raw(0),
            Raw(if valid { negative } else { 0 }),
            Raw(if valid { positive } else { 0 }),
            Raw(u16::from(valid)),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::{FIRST_REGISTER, READING_LIFETIME, command};
    use crate::growatt::v7::encode::Command;
    use crate::model::Raw;

    /// The four raw values a reading encodes to.
    fn values(watts: i32, valid: bool) -> Vec<Raw> {
        let Command::WriteRange { start, values } = command(watts, valid).expect("in range") else {
            panic!("a reading is a range write");
        };
        assert_eq!(start, FIRST_REGISTER);
        values
    }

    #[test]
    fn import_carries_the_magnitude_in_the_upper_register() {
        assert_eq!(values(250, true), vec![Raw(0), Raw(0), Raw(250), Raw(1)]);
    }

    #[test]
    fn export_carries_it_in_the_lower_one() {
        // Direction is which register holds it, not a sign — the whole reason a caller passes a signed
        // figure and this does the splitting.
        assert_eq!(values(-400, true), vec![Raw(0), Raw(400), Raw(0), Raw(1)]);
    }

    #[test]
    fn an_invalid_reading_is_four_zeros() {
        // What the firmware writes for a meter that is not answering, so it is also how a reading is
        // withdrawn. The magnitude is discarded rather than kept alongside a cleared flag.
        assert_eq!(values(250, false), vec![Raw(0), Raw(0), Raw(0), Raw(0)]);
    }

    #[test]
    fn zero_is_a_valid_reading_and_not_an_absent_one() {
        // A meter reading zero means the grid is balanced, which the device acts on by holding its
        // output. That is a different instruction from having no meter.
        assert_eq!(values(0, true), vec![Raw(0), Raw(0), Raw(0), Raw(1)]);
    }

    #[test]
    fn a_figure_beyond_a_register_is_refused_rather_than_truncated() {
        assert!(command(70_000, true).is_err());
        assert!(command(-70_000, true).is_err());
    }

    #[test]
    fn the_lifetime_is_the_device_s_and_not_a_choice_of_ours() {
        // Documented because a publisher has to write inside it. Nothing in this program acts on it: the
        // expiry is what makes a dead publisher safe, so it is deliberately not worked around.
        assert_eq!(READING_LIFETIME.as_secs(), 120);
    }
}
