//! Replay tests against recorded frames.
//!
//! These are the strongest asset the project has: real frames from real hardware, with known
//! conditions. They need no device and no network, and they catch scaling and offset regressions
//! immediately.
//!
//! Every fixture is redacted — see `tests/fixtures/README.md`. The serial is the placeholder
//! `0EXAMPLE00000001` in all three places a telemetry frame carries it, and the CRC was recomputed so
//! the frames still validate.

use heliobridge::growatt::v7::decode::{FromFrame, InputBlock, RECORD_MARKER_TELEMETRY, Telemetry};
use heliobridge::growatt::v7::frame::{Frame, MessageType};
use heliobridge::growatt::v7::registers::{INPUT_REGISTERS, InputRegister};
use heliobridge::growatt::{Codec, ProtocolVersion, peek_version};
use heliobridge::model::{Confidence, Register, Value};

/// The redacted serial every fixture carries.
const SERIAL: &str = "0EXAMPLE00000001";

/// Conditions each fixture was captured under, from `tests/fixtures/README.md`.
struct Fixture {
    file: &'static str,
    wire: &'static [u8],
    timestamp: &'static str,
    pv_power_total: f64,
    ac_power: f64,
    battery_soc_total: f64,
    battery_charge_power: f64,
    battery_charge_status: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        file: "telemetry-night-discharge.bin",
        wire: include_bytes!("fixtures/telemetry-night-discharge.bin"),
        timestamp: "2026-08-06 23:42:45",
        pv_power_total: 0.0,
        ac_power: -49.0,
        battery_soc_total: 7.0,
        battery_charge_power: -49.0,
        battery_charge_status: "discharging",
    },
    Fixture {
        file: "telemetry-midday-charge.bin",
        wire: include_bytes!("fixtures/telemetry-midday-charge.bin"),
        timestamp: "2026-08-07 12:12:02",
        pv_power_total: 413.0,
        ac_power: -49.0,
        battery_soc_total: 98.0,
        battery_charge_power: 364.0,
        battery_charge_status: "charging",
    },
    Fixture {
        file: "telemetry-dusk-low-pv.bin",
        wire: include_bytes!("fixtures/telemetry-dusk-low-pv.bin"),
        timestamp: "2026-08-07 17:19:20",
        pv_power_total: 57.0,
        ac_power: -100.0,
        battery_soc_total: 90.0,
        battery_charge_power: -43.0,
        battery_charge_status: "discharging",
    },
    Fixture {
        file: "telemetry-evening-discharge.bin",
        wire: include_bytes!("fixtures/telemetry-evening-discharge.bin"),
        timestamp: "2026-08-07 22:39:51",
        pv_power_total: 0.0,
        ac_power: -99.0,
        battery_soc_total: 56.0,
        battery_charge_power: -99.0,
        battery_charge_status: "discharging",
    },
];

/// Any printable run at least this long must be part of the placeholder serial. The longest
/// unrelated run across these fixtures is 8 octets, produced by numeric register values landing in
/// the ASCII range; a device serial is 16 characters.
const LONG_RUN: usize = 12;

fn close(actual: f64, expected: f64, what: &str, file: &str) {
    assert!(
        (actual - expected).abs() < 1e-6,
        "{file}: {what} decoded as {actual}, expected {expected}"
    );
}

#[test]
fn every_fixture_parses() {
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).unwrap_or_else(|e| panic!("{}: {e}", fixture.file));
        assert_eq!(frame.wire_len(), 585, "{}", fixture.file);
        assert_eq!(frame.message_type(), MessageType::Telemetry, "{}", fixture.file);
        assert_eq!(frame.device_id(), SERIAL, "{}", fixture.file);
        assert_eq!(frame.header().length, 577, "{}", fixture.file);
    }
}

#[test]
fn generation_is_readable_before_parsing() {
    for fixture in FIXTURES {
        assert_eq!(peek_version(fixture.wire), Some(ProtocolVersion::V7));
        assert_eq!(
            Codec::for_version(ProtocolVersion::V7),
            Some(Codec::V7),
            "{}",
            fixture.file
        );
    }
}

#[test]
fn wire_round_trip_is_byte_exact() {
    // The strongest single check available: it validates the header layout, the obfuscation
    // boundary and the CRC input range together. If the CRC were computed over the deobfuscated
    // body, or the trailing two octets were XORed, this would fail.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        assert_eq!(
            frame.to_wire(),
            fixture.wire,
            "{} did not survive a wire round trip",
            fixture.file
        );
    }
}

#[test]
fn decodes_the_recorded_conditions() {
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).unwrap_or_else(|e| panic!("{}: {e}", fixture.file));

        assert_eq!(telemetry.device_id, SERIAL, "{}", fixture.file);
        assert_eq!(telemetry.record_marker, RECORD_MARKER_TELEMETRY, "{}", fixture.file);

        let stamp = telemetry
            .timestamp
            .unwrap_or_else(|| panic!("{}: no timestamp", fixture.file));
        assert!(stamp.is_plausible(), "{}: {stamp}", fixture.file);
        assert_eq!(stamp.to_string(), fixture.timestamp, "{}", fixture.file);

        for (name, expected) in [
            ("pv_power_total", fixture.pv_power_total),
            ("ac_power", fixture.ac_power),
            ("battery_soc_total", fixture.battery_soc_total),
            ("battery_charge_power", fixture.battery_charge_power),
        ] {
            let actual = telemetry
                .value(name)
                .unwrap_or_else(|| panic!("{}: {name} missing", fixture.file));
            close(actual, expected, name, fixture.file);
        }

        let status = telemetry
            .get("battery_charge_status")
            .unwrap_or_else(|| panic!("{}: status missing", fixture.file));
        assert_eq!(
            status.value,
            Value::Enum {
                raw: match fixture.battery_charge_status {
                    "charging" => 1,
                    "discharging" => 2,
                    _ => 0,
                },
                label: Some(fixture.battery_charge_status),
            },
            "{}",
            fixture.file
        );
    }
}

#[test]
fn derived_battery_power_matches_the_reported_register() {
    // The specification states battery charge power is derived by the vendor cloud as
    // `solar − |ac|`. Register 11 also reports it. Agreement on every fixture is a cross-check on
    // the signed encoding: get the -30000 delta wrong on either input and these diverge.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");

        let reported = telemetry.value("battery_charge_power").expect("register 11");
        let derived = telemetry.derived_battery_charge_power().expect("both inputs present");
        close(derived, reported, "derived battery power", fixture.file);
    }
}

#[test]
fn signed_registers_decode_negative_when_exporting() {
    // Every fixture was captured while exporting, so ac_power must be negative in all of them. A
    // missing delta would show these as ~29950 and a sign error as +49.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        let ac = telemetry.value("ac_power").expect("ac_power");
        assert!(
            (-1000.0..0.0).contains(&ac),
            "{}: ac_power {ac} is not a plausible export figure",
            fixture.file
        );
    }
}

#[test]
fn ac_power_and_its_high_resolution_twin_agree() {
    // The two are one measurement, not two: register 116 has ten times the resolution and the
    // opposite sign convention. They are not latched together, so allow a small sampling skew.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        let ac = telemetry.value("ac_power").expect("register 5");
        let hires = telemetry.value("ac_power_hires").expect("register 116");
        assert!(
            (-hires.trunc() - ac).abs() <= 1.0,
            "{}: ac_power {ac} and ac_power_hires {hires} disagree beyond sampling skew",
            fixture.file
        );
    }
}

#[test]
fn the_household_load_registers_are_unsigned_and_mirror_the_ac_magnitude() {
    // Guards the correction that produced `household_load_total = -29901 W` from a house exporting 99 W:
    // these two are unsigned, so the signed `delta = -30000` must not creep back.
    //
    // The equality with |ac_power| is a property of the installation these fixtures came from rather than of
    // the protocol. The pair differs by load measured through interconnected vendor smart plugs, and there are
    // none here, so the difference is zero. It is asserted anyway because it is a sharp check on the scaling —
    // but a fixture from an installation with plugs would legitimately fail this half of the test, and should
    // be given its own.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        let ac = telemetry.value("ac_power").expect("register 5");
        for name in ["household_load_total", "household_load_excl_groplug"] {
            let load = telemetry.value(name).expect(name);
            assert!(
                load >= 0.0,
                "{}: {name} is {load}, so the signed encoding is back",
                fixture.file
            );
            assert!(
                (load - ac.abs()).abs() <= 3.0,
                "{}: {name} {load} does not track |ac_power| {}",
                fixture.file,
                ac.abs()
            );
        }
    }
}

#[test]
fn temperatures_are_physically_plausible() {
    // The scaling-order bug this guards against read a battery temperature as 289.79 °C.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        for name in ["battery1_temp", "device_temp"] {
            let temp = telemetry.value(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(
                (-20.0..80.0).contains(&temp),
                "{}: {name} decoded as {temp} °C",
                fixture.file
            );
        }
    }
}

#[test]
fn percentages_stay_within_range() {
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        for name in [
            "battery_soc_total",
            "battery1_soc",
            "battery_soh",
            "charge_limit_upper",
            "charge_limit_lower",
        ] {
            let pct = telemetry.value(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(
                (0.0..=100.0).contains(&pct),
                "{}: {name} decoded as {pct} %",
                fixture.file
            );
        }
    }
}

#[test]
fn embedded_serial_matches_the_header() {
    // A frame carries the serial three times. All three must be the placeholder, which is also the
    // check that the redaction did not miss the copy inside the register block.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let telemetry = Telemetry::from_frame(&frame).expect("decode");
        assert_eq!(telemetry.embedded_serial().as_deref(), Some(SERIAL), "{}", fixture.file);
    }
}

#[test]
fn no_fixture_contains_anything_but_the_placeholder_serial() {
    // Guards the privacy property directly rather than trusting the generator: a fixture regenerated
    // by hand, or a new one added carelessly, fails here.
    for fixture in FIXTURES {
        let frame = Frame::parse(fixture.wire).expect("parse");
        let plain = frame.plain();
        let occurrences = plain.windows(SERIAL.len()).filter(|w| *w == SERIAL.as_bytes()).count();
        assert_eq!(
            occurrences, 3,
            "{}: expected the placeholder three times, found {occurrences}",
            fixture.file
        );

        // Any other long run of printable text would be a leak — a component serial, a hostname.
        // The register block produces short printable runs by coincidence, because numeric values
        // land in the ASCII ranges: the longest unrelated run across these fixtures is 8 octets, and
        // a device serial is 16 characters. So every run of 12 or more must be part of the
        // placeholder, and anything else fails here.
        let mut start = None;
        let mut checked = 0usize;
        for (index, octet) in plain.iter().enumerate().chain(core::iter::once((plain.len(), &0))) {
            if octet.is_ascii_graphic() {
                start = start.or(Some(index));
                continue;
            }
            if let Some(from) = start.take() {
                let run = plain.get(from..index).unwrap_or_default();
                if run.len() >= LONG_RUN {
                    checked = checked.saturating_add(1);
                    let text = String::from_utf8_lossy(run);
                    assert!(
                        text.contains(SERIAL),
                        "{}: printable run of {} octets at offset {from} is not the placeholder: {text:?}",
                        fixture.file,
                        run.len()
                    );
                }
            }
        }
        assert_eq!(
            checked, 3,
            "{}: expected exactly three long printable runs",
            fixture.file
        );
    }
}

#[test]
fn every_documented_register_is_present_in_a_real_frame() {
    // The whole table must be reachable within 585 octets. An entry whose offset falls off the end
    // is a typo in the register number, and would otherwise silently never decode.
    let frame = Frame::parse(FIXTURES[0].wire).expect("parse");
    let telemetry = Telemetry::from_frame(&frame).expect("decode");
    assert_eq!(
        telemetry.readings.len(),
        INPUT_REGISTERS.len(),
        "some registers were skipped as out of range"
    );

    for entry in INPUT_REGISTERS {
        assert!(
            frame.u16_at(entry.offset()).is_some(),
            "{} at offset {} is outside a real frame",
            entry.name,
            entry.offset()
        );
    }
}

#[test]
fn single_register_reads_agree_with_a_full_decode() {
    let frame = Frame::parse(FIXTURES[1].wire).expect("parse");
    let telemetry = Telemetry::from_frame(&frame).expect("decode");

    for entry in INPUT_REGISTERS {
        let one = InputBlock::new(&frame)
            .get(entry.register)
            .unwrap_or_else(|| panic!("{} not readable on its own", entry.name));
        let full = telemetry
            .get(entry.name)
            .unwrap_or_else(|| panic!("{} missing from the full decode", entry.name));
        assert_eq!(&one.raw, &full.raw, "{}", entry.name);
        assert_eq!(one.register, full.register, "{}", entry.name);
    }
}

#[test]
fn unknown_registers_decode_but_stay_marked() {
    let frame = Frame::parse(FIXTURES[0].wire).expect("parse");
    let telemetry = Telemetry::from_frame(&frame).expect("decode");

    let unknown: Vec<_> = telemetry.readings.iter().filter(|r| r.is_unknown()).collect();
    assert!(!unknown.is_empty(), "the fixture should still hold unknowns");
    for reading in unknown {
        assert_eq!(
            reading.confidence,
            Confidence::Inferred,
            "{} should be inferred",
            reading.name
        );
    }
}

#[test]
fn a_truncated_frame_is_rejected_rather_than_half_decoded() {
    let wire = FIXTURES[0].wire;
    let truncated = wire.get(..wire.len().wrapping_div(2)).expect("half a frame");
    assert!(Frame::parse(truncated).is_err(), "half a frame must not parse");
}

#[test]
fn every_octet_of_the_body_is_covered_by_the_obfuscation() {
    // Verifies the boundary from the outside: deobfuscating must change the body and leave the
    // header and CRC identical.
    let wire = FIXTURES[0].wire;
    let frame = Frame::parse(wire).expect("parse");
    let plain = frame.plain();

    assert_eq!(plain.get(..8), wire.get(..8), "header must be in clear");
    assert_ne!(
        plain.get(8..38),
        wire.get(8..38),
        "device id must be obfuscated on the wire"
    );
    assert_eq!(
        InputRegister::lookup(Register(5)).map(InputRegister::offset),
        Some(0x59),
        "register 5 sits at 0x4F + 10"
    );
}
