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
use heliobridge::growatt::v7::registers::{Availability, CONFIG_REGISTER_LAST, CONFIG_REGISTERS, ConfigRegister, Role};
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
    // A hostname, which is why the field is not called an IP address.
    assert_eq!(report.get("server_address"), Some("mqtt.growatt.com"));
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
fn the_static_network_fields_do_not_describe_the_live_network() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");
    // The static configuration, which `dhcp_disabled` selects between and this device does not use: it
    // reads 192.168.5.1 while the device is addressed elsewhere entirely. Inert, so nothing tries to
    // reach the device with it.
    assert_eq!(report.get("static_network_ip"), Some("192.168.5.1"));
    assert_eq!(report.get("static_network_gateway"), Some("192.168.5.1"));
    for name in ["static_network_ip", "static_network_gateway", "static_network_mask"] {
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
    // Register 102 reads "DEV:" like the accessory list at 122, and is still undocumented: nothing
    // establishes what it lists, so it is carried by number.
    assert!(unknown.contains(&102), "got {unknown:?}");
}

#[test]
fn the_report_does_not_enumerate_every_writable_register() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");

    // Registers 32 (restart) and 35 (clear log) are absent from the report, and both were later captured
    // being written by the vendor's own web interface. So absence here says nothing about writability, and
    // gating an action on its register appearing would refuse a command that works.
    assert!(!report.reports(Register(32)));
    assert!(!report.reports(Register(35)));
}

#[test]
fn availability_matches_what_the_device_actually_reports() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");

    // Each entry's own declaration, checked against real device bytes in both directions. This lives on the
    // definition rather than in a list beside the tests: the map is where a register gets added, and a
    // separate list is a second place to forget.
    for entry in CONFIG_REGISTERS {
        let reported = report.reports(entry.register);
        let (number, name) = (entry.register.number(), entry.name);
        match entry.availability {
            Availability::Reported => assert!(
                reported,
                "config {number} ({name}) is declared Reported but this device does not volunteer it"
            ),
            Availability::OnRequest => assert!(
                !reported,
                "config {number} ({name}) is declared OnRequest but appears in the report"
            ),
        }
    }
}

#[test]
fn the_report_is_a_minority_of_the_space() {
    let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
    let report = Identity::from_frame(&frame).expect("the identity report decodes");

    // 32 volunteered out of the 146 that exist. That gap is the whole reason reading the space is an
    // operation rather than a matter of waiting for a report.
    assert_eq!(report.entries.len(), 32);
    assert_eq!(CONFIG_REGISTER_LAST, 145);
    let on_request = CONFIG_REGISTERS
        .iter()
        .filter(|entry| entry.availability == Availability::OnRequest)
        .count();
    assert!(
        on_request > 0,
        "the map names registers the device never volunteers; losing that would hide them"
    );
}

/// The four commands the vendor's web interface issued, captured while being refused.
///
/// Each is a `0x18` config write under address `0x01`, which this build classified as unrecognised until it
/// learned that the address distinguishes the originator rather than the scope. Bytes are the deobfuscated
/// bodies as captured; the serial they arrived with is not part of what is asserted.
#[test]
fn config_writes_arrive_under_either_address() {
    use heliobridge::driver::arbiter::{Direction, Intent};
    use heliobridge::growatt::v7::frame::MessageType;

    // register 32 = "1" (restart), 35 = "1" (clear log), 19 = the domain, 18 = the port.
    let cases: [(u8, &[u8], u16, &str); 5] = [
        (0x01, &[0x00, 0x01, 0x00, 0x05, 0x00, 0x20, 0x00, 0x01, b'1'], 32, "1"),
        (0x01, &[0x00, 0x01, 0x00, 0x05, 0x00, 0x23, 0x00, 0x01, b'1'], 35, "1"),
        (
            0x01,
            b"\x00\x01\x00\x14\x00\x13\x00\x10mqtt.growatt.com",
            19,
            "mqtt.growatt.com",
        ),
        (0x01, b"\x00\x01\x00\x08\x00\x12\x00\x047006", 18, "7006"),
        // And the vendor's own clock push, which arrives under the other address.
        (
            0xFE,
            b"\x00\x01\x00\x17\x00\x1f\x00\x132026-08-08 19:09:24",
            31,
            "2026-08-08 19:09:24",
        ),
    ];

    for (address, body, register, value) in cases {
        let wire = frame_with(address, 0x18, body);
        let frame = Frame::parse(&wire).expect("a captured frame parses");
        assert_eq!(
            frame.message_type(),
            MessageType::ConfigWrite,
            "address {address:#04x} was not recognised as a config write"
        );
        assert_eq!(
            frame.intent(Direction::ToDevice),
            Intent::WriteConfig {
                register: Register(register)
            },
            "address {address:#04x}, register {register}"
        );

        // The entry layout the specification states, on octets nobody wrote by hand.
        let entry_len = u16::from_be_bytes([body[2], body[3]]);
        let value_len = u16::from_be_bytes([body[6], body[7]]);
        assert_eq!(entry_len, value_len + 4, "register {register}");
        assert_eq!(&body[8..], value.as_bytes(), "register {register}");
    }
}

#[test]
fn the_answer_to_a_config_read_decodes_as_a_one_entry_report() {
    // The shape the device actually replied with: address 0x01 rather than the report's 0xFE, and a body
    // that is the report's own layout carrying the single register asked for. Values here are synthetic.
    let body = [
        0x00, 0x01, // count: one entry
        0x00, // the same pad the full report carries
        0x00, 0x15, // register 21
        0x00, 0x07, // seven octets of value
        b'9', b'.', b'9', b'.', b'9', b'.', b'9',
    ];
    let wire = frame_with(0x01, 0x19, &body);
    let frame = Frame::parse(&wire).expect("the hand-built frame parses");
    assert_eq!(frame.message_type(), MessageType::ConfigRead);

    let answer = Identity::from_frame(&frame).expect("an answer decodes like a report");
    assert_eq!(answer.declared, 1);
    assert!(!answer.truncated, "the body is consumed exactly");
    assert_eq!(answer.get("sw_version"), Some("9.9.9.9"));
}

#[test]
fn an_answer_replaces_only_the_register_it_carries() {
    let mut report = {
        let frame = Frame::parse(FULL_REPORT).expect("the fixture parses");
        Identity::from_frame(&frame).expect("the identity report decodes")
    };
    let before = report.entries.len();
    let model = report.get("model_id").map(str::to_owned);

    let answer = {
        let body = [
            0x00, 0x01, 0x00, 0x00, 0x15, 0x00, 0x07, b'9', b'.', b'9', b'.', b'9', b'.', b'9',
        ];
        let wire = frame_with(0x01, 0x19, &body);
        let frame = Frame::parse(&wire).expect("the hand-built frame parses");
        Identity::from_frame(&frame).expect("an answer decodes like a report")
    };
    report.apply(&answer);

    assert_eq!(
        report.get("sw_version"),
        Some("9.9.9.9"),
        "the answered register is fresh"
    );
    assert_eq!(report.get("model_id"), model.as_deref(), "the rest survives");
    assert_eq!(report.entries.len(), before, "a merge is not an append");
}

/// Build a frame with an arbitrary address, which `Frame::new` cannot do — it derives the address from the
/// message type, and the point here is a message type arriving under an address this program never sends.
fn frame_with(address: u8, function: u8, body: &[u8]) -> Vec<u8> {
    const SERIAL: &[u8] = b"0EXAMPLE00000001";
    const KEY: &[u8; 7] = b"Growatt";

    let mut plain = Vec::new();
    plain.extend_from_slice(&1u16.to_be_bytes());
    plain.extend_from_slice(&7u16.to_be_bytes());
    // Length counts everything after itself except the CRC: address, function, device ID, body.
    let length = u16::try_from(body.len().saturating_add(32)).unwrap_or(u16::MAX);
    plain.extend_from_slice(&length.to_be_bytes());
    plain.push(address);
    plain.push(function);
    plain.extend_from_slice(SERIAL);
    plain.extend_from_slice(&[0; 14]);
    plain.extend_from_slice(body);

    // Obfuscate from offset 8, with the key phase restarting there. `zip` over a cycling key rather than an
    // index, so there is no subtraction to get wrong and nothing to index out of range.
    let mut wire = plain;
    for (octet, key) in wire.iter_mut().skip(8).zip(KEY.iter().cycle()) {
        *octet ^= *key;
    }
    let crc = crc16_modbus(&wire);
    wire.extend_from_slice(&crc.to_be_bytes());
    wire
}

/// CRC-16/MODBUS, so the hand-built frames above validate like any other.
fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for octet in data {
        crc ^= u16::from(*octet);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}
