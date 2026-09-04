//! The product's full firmware version, assembled from the parts the device reports.
//!
//! The vendor identifies a release by six fields — `15.12.17.15.9000.4026` — one per component, and its
//! update service keys downloads on the same digits packed behind a device type code. The device never
//! reports that string; it reports the pieces, in two different address spaces:
//!
//! | Field | Where |
//! |---|---|
//! | inverter | input register 119, high octet |
//! | MPPT | input register 119, low octet |
//! | PD | input register 120, high octet |
//! | BMS | input register 120, low octet |
//! | CT | not reported — the constant below |
//! | datalogger | config register 21, `sw_version`, with the dots removed |
//!
//! So assembling it needs a telemetry frame *and* the identity report, which is why this is its own type
//! rather than a method on either.

use core::fmt;

use crate::model::Raw;

/// The CT field, which no observed release varies.
///
/// Every version string published for this product family carries `9000` here, and nothing the device
/// sends corresponds to it. Treated as a constant rather than reported, and marked as an assumption in
/// the specification: a release that changed it would make this string wrong in that field alone.
pub const CT_VERSION: u16 = 9000;

/// A full firmware version, one field per component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareVersion {
    /// Inverter.
    pub inverter: u8,
    /// MPPT.
    pub mppt: u8,
    /// Power-distribution controller.
    pub pd: u8,
    /// Battery management.
    pub bms: u8,
    /// CT — always [`CT_VERSION`].
    pub ct: u16,
    /// Datalogger, as the digits of its dotted version: `4.0.1.9` becomes `4019`.
    pub datalogger: String,
}

impl FirmwareVersion {
    /// Assemble from the two telemetry registers and the datalogger's own version string.
    ///
    /// Returns `None` when the datalogger version holds no digits, which is the only way any part of this
    /// can be missing — the two registers are always present in a frame that decoded at all.
    pub fn assemble(inverter_mppt: Raw, pd_bms: Raw, datalogger: &str) -> Option<Self> {
        let digits: String = datalogger.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        // Each octet is one component's version, so the halves are taken rather than truncated.
        let [inverter, mppt] = inverter_mppt.get().to_be_bytes();
        let [pd, bms] = pd_bms.get().to_be_bytes();
        Some(Self {
            inverter,
            mppt,
            pd,
            bms,
            ct: CT_VERSION,
            datalogger: digits,
        })
    }
}

impl fmt::Display for FirmwareVersion {
    /// The dotted form the vendor and its users write, with each component zero-padded to two digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            inverter,
            mppt,
            pd,
            bms,
            ct,
            datalogger,
        } = self;
        write!(f, "{inverter:02}.{mppt:02}.{pd:02}.{bms:02}.{ct}.{datalogger}")
    }
}

#[cfg(test)]
mod tests {
    use super::{CT_VERSION, FirmwareVersion};
    use crate::model::Raw;

    #[test]
    fn the_reference_device_assembles_to_a_published_release() {
        // 0x0E0C and 0x0E0B are what this device reports, and 14.12.14.11.9000.4019 appears in an
        // independent list of releases seen in the wild — which is what identifies these registers at all.
        let version = FirmwareVersion::assemble(Raw(0x0E0C), Raw(0x0E0B), "4.0.1.9").expect("assembles");
        assert_eq!(version.to_string(), "14.12.14.11.9000.4019");
        assert_eq!(version.inverter, 14);
        assert_eq!(version.mppt, 12);
        assert_eq!(version.pd, 14);
        assert_eq!(version.bms, 11);
        assert_eq!(version.ct, CT_VERSION);
    }

    #[test]
    fn each_component_is_padded_to_two_digits() {
        // The vendor writes 09.05.05.04.9000.4014, not 9.5.5.4. A consumer comparing strings against a
        // release list would miss every single-digit component otherwise.
        let version = FirmwareVersion::assemble(Raw(0x0905), Raw(0x0504), "4.0.1.4").expect("assembles");
        assert_eq!(version.to_string(), "09.05.05.04.9000.4014");
    }

    #[test]
    fn the_datalogger_field_is_the_digits_of_its_own_version() {
        let version = FirmwareVersion::assemble(Raw(0x0F0C), Raw(0x1110), "4.0.2.6").expect("assembles");
        assert_eq!(version.datalogger, "4026");
        assert_eq!(version.to_string(), "15.12.17.16.9000.4026");
    }

    #[test]
    fn a_datalogger_version_with_no_digits_yields_nothing() {
        // Rather than a string with an empty field, which would look like a real version and compare
        // equal to nothing.
        assert!(FirmwareVersion::assemble(Raw(0x0E0C), Raw(0x0E0B), "").is_none());
        assert!(FirmwareVersion::assemble(Raw(0x0E0C), Raw(0x0E0B), "unknown").is_none());
    }

    #[test]
    fn a_zero_component_is_kept_rather_than_dropped() {
        // The AURA's MPPT field is 00 in its published version, so zero is a real value here.
        let version = FirmwareVersion::assemble(Raw(0x0900), Raw(0x0A05), "4.0.2.8").expect("assembles");
        assert_eq!(version.to_string(), "09.00.10.05.9000.4028");
    }
}
