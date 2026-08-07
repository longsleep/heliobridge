//! Byte-exactness tests against frames the **vendor server** actually sent.
//!
//! This is the strongest check available on the encoder. Self-consistency — build a frame, parse it
//! back — proves the implementation agrees with itself. Byte equality against a captured vendor frame
//! proves it agrees with the thing it is replacing, including the parts nobody understands: the
//! composite range write that carries a zero into register 321, and the unexplained prefix of the time
//! push.
//!
//! Fixtures are redacted; see `tests/fixtures/README.md`.

// `clippy.toml` relaxes the panic lints inside `#[test]` functions, but this file has a shared helper
// that is not itself a test, and a failed `expect` there is the same readable failure as anywhere else
// in a test binary.
#![expect(clippy::expect_used, reason = "test helper; a panic is the reporting mechanism")]

use heliobridge::growatt::v7::encode::{Command, SlotConfig};
use heliobridge::growatt::v7::frame::{Frame, MessageType};
use heliobridge::growatt::v7::{Telemetry, Timestamp};
use heliobridge::model::Register;

/// The redacted serial every fixture carries, and the one the encoder must be given to reproduce them.
const SERIAL: &str = "0EXAMPLE00000001";

const WRITE_SINGLE: &[u8] = include_bytes!("fixtures/write-single-grid-power-allowed.bin");
const WRITE_RANGE: &[u8] = include_bytes!("fixtures/write-range-charge-limits.bin");
const WRITE_POWER: &[u8] = include_bytes!("fixtures/write-range-default-output-power.bin");
const WRITE_SLOT: &[u8] = include_bytes!("fixtures/write-range-slot1.bin");
const TIME_PUSH: &[u8] = include_bytes!("fixtures/time-push.bin");

/// Assert an encoded command is byte-identical to a captured vendor frame.
fn assert_matches_vendor(command: &Command, captured: &[u8], what: &str) {
    let built = command.to_frame(SERIAL).expect("build");
    let wire = built.to_wire();

    // Compare the parsed forms first: a mismatch there gives a readable diff, where a raw byte
    // comparison would only say the vectors differ.
    let expected = Frame::parse(captured).expect("the captured frame must parse");
    assert_eq!(built.message_type(), expected.message_type(), "{what}: message type");
    assert_eq!(built.body(), expected.body(), "{what}: body");
    assert_eq!(built.header(), expected.header(), "{what}: header");
    assert_eq!(wire, captured, "{what}: encoded frame differs from the captured one");
}

#[test]
fn single_register_write_is_byte_identical() {
    // Vendor frame: grid_power_allowed (326) = 1.
    let command = Command::write(Register(326), 1).expect("326 is writable");
    assert_matches_vendor(&command, WRITE_SINGLE, "write single 326");
    assert_eq!(WRITE_SINGLE.len(), 44);
}

#[test]
fn range_write_is_byte_identical() {
    // Vendor frame: charge_limit_upper = 100, charge_limit_lower = 5, as one range.
    let command = Command::write_range(&[(Register(250), 100), (Register(251), 5)]).expect("writable");
    assert_matches_vendor(&command, WRITE_RANGE, "write range 250..251");
    assert_eq!(WRITE_RANGE.len(), 48);
}

#[test]
fn default_output_power_composite_is_byte_identical() {
    // The one that matters most. The vendor writes 322 as a range starting at the unknown register
    // 321, with a zero in it. An implementation that writes 322 on its own produces a different frame
    // — this test is what pins the behaviour to what the hardware has actually been observed to accept.
    let command = Command::set_default_output_power(1000).expect("in range");
    assert_matches_vendor(&command, WRITE_POWER, "set default_output_power 1000");
    assert_eq!(WRITE_POWER.len(), 48);

    // And confirm the zero really is in the frame rather than an artefact of the comparison.
    let built = command.to_frame(SERIAL).expect("build");
    assert_eq!(built.body(), [0x01, 0x41, 0x01, 0x42, 0x00, 0x00, 0x03, 0xE8]);
}

#[test]
fn slot_write_is_byte_identical() {
    // Vendor frame: slot 1 = 00:00-23:59, load first, 50 W, enabled.
    let command = Command::set_slot(
        1,
        SlotConfig {
            start_hour: 0,
            start_minute: 0,
            end_hour: 23,
            end_minute: 59,
            work_mode: 0,
            output_power: 50,
            enabled: true,
        },
    )
    .expect("slot 1 exists");
    assert_matches_vendor(&command, WRITE_SLOT, "set slot 1");
    assert_eq!(WRITE_SLOT.len(), 54);
}

#[test]
fn time_push_is_byte_identical() {
    // Including the eight-octet prefix whose first three pairs remain unexplained. Reproducing bytes
    // nobody understands is exactly why this test compares against a vendor frame.
    let command = Command::time_push(Timestamp {
        year: 2026,
        month: 8,
        day: 6,
        hour: 23,
        minute: 43,
        second: 2,
    })
    .expect("plausible");
    assert_matches_vendor(&command, TIME_PUSH, "time push");
    assert_eq!(TIME_PUSH.len(), 67);
}

#[test]
fn every_vendor_fixture_parses_and_round_trips() {
    for (name, wire) in [
        ("write-single", WRITE_SINGLE),
        ("write-range", WRITE_RANGE),
        ("write-power", WRITE_POWER),
        ("write-slot", WRITE_SLOT),
        ("time-push", TIME_PUSH),
    ] {
        let frame = Frame::parse(wire).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(frame.device_id(), SERIAL, "{name}");
        assert_eq!(frame.to_wire(), wire, "{name} did not survive a round trip");
        // The length rule holds on server-originated frames too, not only telemetry.
        assert!(frame.header().length_matches(wire.len()), "{name}");
    }
}

#[test]
fn server_frames_are_not_telemetry() {
    // Guards against a decoder being pointed at the wrong direction and silently producing readings
    // from a write frame's body.
    for (name, wire) in [("write-single", WRITE_SINGLE), ("time-push", TIME_PUSH)] {
        let frame = Frame::parse(wire).expect("parse");
        assert_ne!(frame.message_type(), MessageType::Telemetry, "{name}");
        assert!(
            Telemetry::try_from(&frame).is_err(),
            "{name} must not decode as telemetry"
        );
    }
}

#[test]
fn the_time_push_is_datalogger_scoped() {
    // Address 0xFE, not 0x01: the time push is addressed to the datalogger rather than the inverter,
    // which is what makes address+function worth reading as one message type.
    let frame = Frame::parse(TIME_PUSH).expect("parse");
    assert_eq!(frame.header().address, 0xFE);
    assert_eq!(frame.message_type(), MessageType::TimePush);
    assert_eq!(frame.message_type().as_u16(), 0xFE18);
}

#[test]
fn no_vendor_fixture_leaks_a_serial() {
    // Server-originated frames carry the serial once, in the device ID field.
    for (name, wire) in [
        ("write-single", WRITE_SINGLE),
        ("write-range", WRITE_RANGE),
        ("write-power", WRITE_POWER),
        ("write-slot", WRITE_SLOT),
        ("time-push", TIME_PUSH),
    ] {
        let frame = Frame::parse(wire).expect("parse");
        let occurrences = frame
            .plain()
            .windows(SERIAL.len())
            .filter(|w| *w == SERIAL.as_bytes())
            .count();
        assert_eq!(
            occurrences, 1,
            "{name}: expected the placeholder exactly once, found {occurrences}"
        );
    }
}
