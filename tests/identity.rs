//! Replay tests for the identity report, against real device bytes.
//!
//! The fixture is one `0xFE19` frame from the capture, redacted twice over: the serial replaced with the
//! placeholder in both places it appears, and the values of config registers 7 and 16 — a password field and
//! a MAC-shaped constant — overwritten with `X` to the same length. Lengths are preserved, so every offset,
//! every TLV length and the frame length are exactly what the device sent, and the CRC was recomputed.
//!
//! Regenerate with:
//!
//! ```text
//! python3 tools/make_fixture.py captures/mqtt-frames.up --func 0x19 --count 1 \
//!     --names identity-report-32-entries --redact-config 7,16 --outdir heliobridge/tests/fixtures
//! ```

use heliobridge::growatt::v7::decode::FromFrame;
use heliobridge::growatt::v7::frame::{Frame, MessageType};
use heliobridge::growatt::v7::identity::{Entry, Identity};
use heliobridge::growatt::v7::registers::{CONFIG_REGISTERS, ConfigRegister, Role};
use heliobridge::model::Register;

/// A full report as the device sends it, 401 octets.
const FULL_REPORT: &[u8] = include_bytes!("fixtures/identity-report-32-entries.bin");

/// The serial every committed fixture carries in place of the real one.
const PLACEHOLDER_SERIAL: &str = "0EXAMPLE00000001";

// Each test parses in two steps rather than through a helper. `Frame::parse` yields `FrameError` and
// `Identity::from_frame` yields `DecodeError`; a helper returning both would have to erase them behind a
// boxed trait object, since neither `snafu` nor `thiserror` is a dev-dependency and adding one to give a
// test helper an error type it never inspects is not worth it. Two lines keep both types exact, and match
// what `tests/replay.rs` does.

#[test]
fn the_fixture_is_an_identity_report() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    assert_eq!(frame.message_type(), MessageType::IdentityReport);
    assert_eq!(frame.wire_len(), 401);
}

#[test]
fn the_declared_count_matches_the_entries_and_consumes_the_body() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    assert_eq!(report.declared, 32, "the leading field is a count, not a subtype");
    assert_eq!(report.entries.len(), 32);
    assert!(
        !report.truncated,
        "32 entries consume the 361-octet body exactly; a leftover means the layout is misread"
    );
}

#[test]
fn the_metadata_a_device_page_needs_is_present() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    assert_eq!(report.get("model_id"), Some("GTSW0000"));
    assert_eq!(report.get("sw_version"), Some("4.0.1.9"));
    assert_eq!(report.get("hw_version"), Some("V1.0"));
    assert_eq!(report.get("protocol_version"), Some("2.0"));
}

#[test]
fn the_endpoint_is_what_the_device_believes_it_should_dial() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    assert_eq!(report.endpoint().as_deref(), Some("mqtt.growatt.com:7006"));
    assert_eq!(report.get("remote_ip"), Some("mqtt.growatt.com"));
    assert_eq!(report.get("remote_port"), Some("7006"));
}

#[test]
fn values_are_ascii_even_where_the_field_is_numeric() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    // A port, an interval and a signal strength all arrive as text. Parsing them as octets is the
    // mistake this asserts against.
    assert_eq!(report.get("remote_port"), Some("7006"));
    assert_eq!(
        report.get("data_interval"),
        Some("5"),
        "5 s, matching the observed cadence"
    );

    // The signal is asserted by shape rather than by value: it and the clock are the only two fields that
    // differ between reports, so pinning the number would make the test a record of which frame was
    // sampled. What matters is that a negative decimal survives as text.
    let signal = report.get("wifi_signal").expect("reported");
    let dbm: i32 = signal.parse().expect("a signed decimal, not octets");
    assert!((-100..0).contains(&dbm), "implausible dBm: {signal}");
}

#[test]
fn the_clock_and_timezone_disagree_as_documented() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    // The device reports GMT+8 while being sent local time by whoever holds the clock. Asserted because
    // an implementation that derived the clock from this field would be eight hours out.
    assert_eq!(report.get("timezone"), Some("GMT+8"));
    assert!(
        report.get("datetime").is_some_and(|t| t.starts_with("2026-08-06")),
        "the capture date"
    );
}

#[test]
fn the_inert_network_fields_do_not_describe_the_live_network() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    // 192.168.5.1 while the device was addressed elsewhere entirely. Marked inert so nothing tries to
    // reach the device with it.
    assert_eq!(report.get("local_ip"), Some("192.168.5.1"));
    assert_eq!(report.get("default_gateway"), Some("192.168.5.1"));
    for name in ["local_ip", "default_gateway", "subnet_mask"] {
        assert_eq!(ConfigRegister::lookup_name(name).expect(name).role, Role::Inert);
    }
}

#[test]
fn the_committed_fixture_carries_no_real_device_data() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");

    // The invariant is about this file, not about what the software reports: a captured frame is redacted
    // before it is committed. The serial reads as the placeholder, and the two fields that carry no
    // model-wide constant were overwritten to the same length.
    assert_eq!(report.get("serial_number"), Some(PLACEHOLDER_SERIAL));
    for name in ["password", "mac_address"] {
        let value = report.get(name).expect(name);
        assert!(
            value.bytes().all(|b| b == b'X'),
            "{name} was not redacted in the fixture: {value}"
        );
    }
}

#[test]
fn notable_entries_are_the_ones_worth_showing() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");

    // Everything is reachable; `notable` only says which are worth a label.
    let notable: Vec<&str> = report.notable().filter_map(Entry::name).collect();
    assert!(notable.contains(&"sw_version"), "{notable:?}");
    assert!(notable.contains(&"remote_port"), "{notable:?}");
    assert!(!notable.contains(&"local_ip"), "inert default: {notable:?}");
    assert!(report.entries.len() > notable.len(), "skipped is not hidden");
}

#[test]
fn unrecognised_keys_are_carried_rather_than_rejected() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    let unknown: Vec<u16> = report
        .entries
        .iter()
        .filter(|entry| entry.definition().is_none())
        .map(|entry| entry.register.number())
        .collect();
    assert!(
        !unknown.is_empty(),
        "this device reports keys outside the documented map; carrying them is how the next one is named"
    );
    // Registers 102 and 122 both read "DEV:" and are documented as unknown in Appendix C.
    assert!(unknown.contains(&122), "got {unknown:?}");
}

#[test]
fn the_restart_register_is_not_reported_by_this_device() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    assert!(
        !report.reports(Register(32)),
        "register 32 is absent, so a restart action keyed to it must not be offered for this model"
    );
}

#[test]
fn every_documented_config_register_is_either_reported_or_deliberately_absent() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    let missing: Vec<&str> = CONFIG_REGISTERS
        .iter()
        .filter(|entry| !report.reports(entry.register))
        .map(|entry| entry.name)
        .collect();
    assert!(
        missing.is_empty(),
        "the map documents registers this device does not report: {missing:?}"
    );
}
