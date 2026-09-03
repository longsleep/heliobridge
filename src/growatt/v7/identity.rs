//! The identity report: what the datalogger says about itself.
//!
//! One `0xFE19` frame per connect, carrying the config registers as a TLV list. The body is a 2-octet
//! entry count, one pad octet, then entries of `register(2) length(2) value`. Values are **ASCII**, whatever
//! the field means — a port arrives as `"7006"`.
//!
//! # There is no subtype: that field is the entry count
//!
//! The specification described the leading `0x0020` as a subtype meaning "full configuration", and `0x0001`
//! as marking a short form. Measured across eight frames, the field is simply how many entries follow —
//! `0x0020` is 32 of them, `0x0001` is one — and in every case the entries consume the body exactly. So there
//! is one message shape, not two, and the "short form" is that message carrying a single register.
//!
//! # A truncated list is what was read, not an error
//!
//! Entries are returned up to the point the body stops making sense, with [`Identity::truncated`] recording
//! that it happened. The alternative — rejecting the frame — throws away the entries that did parse, and
//! this is the one frame that carries the firmware version. A short read is still evidence.
//!
//! # Everything decoded is available
//!
//! Every entry is exposed, including the serial, the password field and the MAC-shaped constant. This is the
//! device owner's own data on their own socket, and a decoder that hid fields would make them unreachable to
//! the only person entitled to them.
//!
//! [`Identity::summary`] exists for the sake of a log line, which wants to be one line rather than thirty-two.
//! It is a convenience, not a filter — [`Identity::entries`] is the whole report.

use core::fmt;

use snafu::ensure;

use crate::growatt::v7::decode::{DecodeError, FromFrame, WrongMessageTypeSnafu};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::v7::registers::{ConfigRegister, Role};
use crate::model::Register;

/// Octets before the first entry: the entry count and one pad.
const PREAMBLE_LEN: usize = 3;

/// Octets of header on each entry: register and length.
const ENTRY_HEADER_LEN: usize = 4;

/// One config register as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The config register number.
    pub register: Register,
    /// The value, as sent. ASCII in every observed case, kept verbatim.
    pub value: String,
}

impl Entry {
    /// The register's documented definition, if it has one.
    pub fn definition(&self) -> Option<&'static ConfigRegister> {
        ConfigRegister::lookup(self.register)
    }

    /// The documented field name, or `None` for a key this build does not know.
    pub fn name(&self) -> Option<&'static str> {
        self.definition().map(|entry| entry.name)
    }

    /// What the field is for, or `None` for a key this build does not know.
    ///
    /// Presentation only — which fields deserve an entity, which are inert defaults not worth showing. It
    /// says nothing about whether a value may be reported; all of them may.
    pub fn role(&self) -> Option<Role> {
        self.definition().map(|entry| entry.role)
    }
}

/// A parsed identity report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// How many entries the frame declared. The leading field of the body; see the module documentation
    /// for why this is not a subtype.
    pub declared: u16,
    /// The entries that parsed.
    pub entries: Vec<Entry>,
    /// Whether the body ran out before the declared count was reached.
    pub truncated: bool,
}

impl Identity {
    /// The value of one config register, if it was reported.
    pub fn value(&self, register: Register) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.register == register)
            .map(|entry| entry.value.as_str())
    }

    /// The value of one config register by its documented name.
    pub fn get(&self, name: &str) -> Option<&str> {
        let entry = ConfigRegister::lookup_name(name)?;
        self.value(entry.register)
    }

    /// Entries worth presenting as their own reading: metadata, live values, the endpoint.
    ///
    /// Skips the inert defaults and the keys this build cannot name — neither is useful as a labelled
    /// entity. Not a privacy boundary: [`Self::entries`] carries everything, and a consumer wanting the
    /// whole report should read that.
    pub fn notable(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.role(), Some(Role::Metadata | Role::Dynamic | Role::Endpoint)))
    }

    /// The endpoint the datalogger believes it should connect to, as `host:port`.
    ///
    /// Worth reading back rather than assuming: it is how a retarget would be confirmed, and how an
    /// unexpected reversion would become visible.
    pub fn endpoint(&self) -> Option<String> {
        let host = self.get("remote_url").or_else(|| self.get("server_address"))?;
        Some(match self.get("remote_port") {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        })
    }

    /// A one-line summary for a log: the fields worth seeing at a glance, not a redaction.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for name in ["model_id", "sw_version", "hw_version", "timezone", "wifi_signal"] {
            if let Some(value) = self.get(name) {
                parts.push(format!("{name}={value}"));
            }
        }
        if let Some(endpoint) = self.endpoint() {
            parts.push(format!("endpoint={endpoint}"));
        }
        parts.push(format!("entries={}", self.entries.len()));
        parts.join(" ")
    }

    /// Fold a later report into this one.
    ///
    /// The answer to a config read carries a single register, and replacing a 32-entry report with it would
    /// discard everything else the device said about itself. Entries are updated in place or appended, and
    /// `declared` keeps describing the report this began as — it is a fact about a frame, not about the
    /// accumulated picture.
    pub fn apply(&mut self, newer: &Self) {
        for entry in &newer.entries {
            match self.entries.iter_mut().find(|held| held.register == entry.register) {
                Some(held) => held.value.clone_from(&entry.value),
                None => self.entries.push(entry.clone()),
            }
        }
    }

    /// Whether a config register this build documents was reported at all.
    ///
    /// The reason this matters: an action keyed to a register the device never mentions should not be
    /// offered. The restart command is the case in point.
    pub fn reports(&self, register: Register) -> bool {
        self.value(register).is_some()
    }
}

impl fmt::Display for Identity {
    /// The summary, so a log line stays one line. See the module documentation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

impl FromFrame for Identity {
    fn from_frame(frame: &Frame) -> Result<Self, DecodeError> {
        // Two message types, one body. An unsolicited report arrives as 0xFE19 with everything the device
        // knows about itself; the answer to a config read arrives as 0x0119 with the one register asked for,
        // laid out identically — count, pad, then TLV entries. Observed: a read of register 21 came back in
        // 54 octets as `00 01 00 | 00 15 00 07 "4.0.1.9"`.
        let actual = frame.message_type();
        ensure!(
            matches!(actual, MessageType::IdentityReport | MessageType::ConfigRead),
            WrongMessageTypeSnafu {
                expected: MessageType::IdentityReport,
                actual,
            }
        );

        let body = frame.body();
        let read_u16 = |at: usize| -> Option<u16> {
            let pair = body.get(at..at.checked_add(2)?)?;
            Some(u16::from_be_bytes(<[u8; 2]>::try_from(pair).ok()?))
        };

        let declared = read_u16(0).unwrap_or_default();
        let mut entries = Vec::new();
        let mut offset = PREAMBLE_LEN;
        let mut truncated = false;

        while entries.len() < usize::from(declared) {
            let Some(register) = read_u16(offset) else {
                truncated = true;
                break;
            };
            let Some(length) = read_u16(offset.saturating_add(2)) else {
                truncated = true;
                break;
            };
            let start = offset.saturating_add(ENTRY_HEADER_LEN);
            let end = start.saturating_add(usize::from(length));
            let Some(value) = body.get(start..end) else {
                truncated = true;
                break;
            };
            entries.push(Entry {
                register: Register(register),
                // Lossy rather than fatal: one unexpected octet in one field must not cost the whole
                // report, which is the only frame carrying the firmware version.
                value: String::from_utf8_lossy(value).into_owned(),
            });
            offset = end;
        }

        Ok(Self {
            declared,
            entries,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Identity, PREAMBLE_LEN};
    use crate::growatt::v7::decode::FromFrame;
    use crate::growatt::v7::frame::{Frame, MessageType};
    use crate::growatt::v7::registers::{ConfigRegister, Role};
    use crate::model::Register;

    const SERIAL: &str = "0EXAMPLE00000001";

    /// Build an identity frame body the way the device does, then wrap and parse it.
    fn identity(count: u16, entries: &[(u16, &str)]) -> Identity {
        let mut body = Vec::new();
        body.extend_from_slice(&count.to_be_bytes());
        body.push(0);
        for (register, value) in entries {
            body.extend_from_slice(&register.to_be_bytes());
            let bytes = value.as_bytes();
            body.extend_from_slice(&u16::try_from(bytes.len()).expect("short").to_be_bytes());
            body.extend_from_slice(bytes);
        }
        let wire = Frame::new(MessageType::IdentityReport, SERIAL, &body)
            .expect("build")
            .to_wire();
        let frame = Frame::parse(&wire).expect("parse");
        Identity::from_frame(&frame).expect("decode")
    }

    #[test]
    fn the_leading_field_is_the_entry_count() {
        assert_eq!(PREAMBLE_LEN, 3, "count and one pad octet");
        let report = identity(1, &[(18, "7006")]);
        assert_eq!(report.declared, 1);
        assert_eq!(report.value(Register(18)), Some("7006"));
        assert!(!report.truncated);
    }

    #[test]
    fn values_are_text_whatever_the_field_means() {
        let report = identity(3, &[(18, "7006"), (76, "-67"), (4, "5")]);
        assert_eq!(report.get("remote_port"), Some("7006"));
        assert_eq!(report.get("wifi_signal"), Some("-67"));
        assert_eq!(report.get("data_interval"), Some("5"));
    }

    #[test]
    fn the_short_form_parses_by_the_same_rules() {
        // Observed after a reconnect: subtype 0x0001, one entry, register 122 with "DEV:".
        let report = identity(1, &[(122, "DEV:")]);
        assert_eq!(report.entries.len(), 1);
        assert!(!report.truncated);
        assert_eq!(report.value(Register(122)), Some("DEV:"));
        assert_eq!(report.entries[0].name(), None, "register 122 is not documented");
    }

    #[test]
    fn a_body_that_stops_early_keeps_what_parsed() {
        // Declares three entries and supplies one: the firmware version is worth more than the objection.
        let mut body = vec![0x00, 0x03, 0x00];
        body.extend_from_slice(&[0x00, 0x15]);
        body.extend_from_slice(&[0x00, 0x04]);
        body.extend_from_slice(b"1.42");
        let wire = Frame::new(MessageType::IdentityReport, SERIAL, &body)
            .expect("build")
            .to_wire();
        let frame = Frame::parse(&wire).expect("parse");
        let report = Identity::from_frame(&frame).expect("decode");

        assert_eq!(report.declared, 3);
        assert_eq!(report.entries.len(), 1);
        assert!(report.truncated, "the shortfall is recorded rather than hidden");
        assert_eq!(report.get("sw_version"), Some("1.42"));
    }

    #[test]
    fn the_endpoint_is_assembled_from_three_registers() {
        let report = identity(3, &[(17, "mqtt.growatt.com"), (18, "7006"), (19, "mqtt.growatt.com")]);
        assert_eq!(report.endpoint().as_deref(), Some("mqtt.growatt.com:7006"));
    }

    #[test]
    fn every_entry_is_reachable_including_the_ones_a_summary_omits() {
        let report = identity(4, &[(8, SERIAL), (20, "GTSW0000"), (21, "1.42"), (76, "-67")]);

        // The summary is short by design, so it leaves fields out.
        let summary = report.summary();
        assert!(summary.contains("GTSW0000"), "{summary}");
        assert!(summary.contains("wifi_signal=-67"), "{summary}");
        assert_eq!(format!("{report}"), summary, "Display is the summary");

        // Everything is still there for a caller that wants it, the serial included.
        assert_eq!(report.value(Register(8)), Some(SERIAL));
        assert_eq!(report.entries.len(), 4);
    }

    #[test]
    fn notable_skips_inert_defaults_and_unnamed_keys() {
        let report = identity(4, &[(20, "GTSW0000"), (14, "192.168.5.1"), (18, "7006"), (999, "?")]);
        let notable: Vec<_> = report.notable().map(|entry| entry.register.number()).collect();
        assert_eq!(
            notable,
            vec![20, 18],
            "an inert default and an unnamed key are not readings"
        );
        // Skipped is not hidden: both are in the entry list.
        assert_eq!(report.entries.len(), 4);
    }

    #[test]
    fn reports_answers_whether_a_register_was_mentioned() {
        let report = identity(1, &[(31, "2026-08-08 16:22:02")]);
        assert!(report.reports(Register(31)));
        assert!(
            !report.reports(Register(32)),
            "the restart register is not offered unless the device mentions it"
        );
    }

    #[test]
    fn the_wrong_message_type_is_refused() {
        let wire = Frame::new(MessageType::Telemetry, SERIAL, &[0; 8])
            .expect("build")
            .to_wire();
        let frame = Frame::parse(&wire).expect("parse");
        assert!(Identity::from_frame(&frame).is_err());
    }

    #[test]
    fn the_documented_map_classifies_by_what_a_field_is_for() {
        // Identity fields are reported like any other; the classification is what tells a fixture
        // generator which values to redact and a device page which to skip.
        for name in ["serial_number", "password", "mac_address"] {
            assert_eq!(ConfigRegister::lookup_name(name).expect(name).role, Role::Identity);
        }
        assert_eq!(
            ConfigRegister::lookup_name("sw_version").expect("known").role,
            Role::Metadata
        );
        assert_eq!(
            ConfigRegister::lookup_name("static_network_ip").expect("known").role,
            Role::Inert
        );
    }
}
