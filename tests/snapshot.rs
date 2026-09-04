//! Replay tests for the hourly settings snapshot, against real device bytes.
//!
//! The fixture is one `0x0103` frame from the capture, redacted three ways over: the serial replaced with
//! the placeholder in all three places it appears, and the component serial numbers the frame carries as
//! ASCII overwritten with `X` to the same length. Lengths are preserved, so every offset and the frame
//! length are exactly what the device sent, and the CRC was recomputed.
//!
//! Regenerate with:
//!
//! ```text
//! python3 tools/make_fixture.py captures/mqtt-frames.up --func 0x03 --count 1 \
//!     --names settings-snapshot --redact-ascii 193 --outdir heliobridge/tests/fixtures
//! ```
//!
//! Why this matters enough to have replay tests: the snapshot is the only message reporting the whole
//! settings space at once, and the only way a change made in the vendor application becomes visible
//! without reconnecting and reading every register back.

use heliobridge::growatt::v7::decode::{FromFrame, SettingsSnapshot};
use heliobridge::growatt::v7::frame::{Frame, MessageType};
use heliobridge::growatt::v7::registers::HoldingRegister;
use heliobridge::model::{Raw, Register};

const SNAPSHOT: &[u8] = include_bytes!("fixtures/settings-snapshot.bin");

/// The value the fixture carries for one register, or `None` if it is not in the snapshot.
fn value(snapshot: &SettingsSnapshot, register: u16) -> Option<u16> {
    snapshot
        .values
        .iter()
        .find(|(number, _)| *number == Register(register))
        .map(|(_, raw)| raw.get())
}

#[test]
fn the_fixture_is_a_settings_snapshot() {
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    assert_eq!(frame.message_type(), MessageType::SettingsSnapshot);
    assert_eq!(frame.wire_len(), 839, "the documented size");
}

#[test]
fn the_frame_declares_the_range_it_carries() {
    // Read from the frame rather than assumed: the two octet pairs before the block hold the first and
    // last register, and a receiver that hard-coded 250 would break on a device that moved them.
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    let snapshot = SettingsSnapshot::from_frame(&frame).expect("it decodes");
    assert_eq!(snapshot.start, Register(250));
    assert_eq!(snapshot.end, Register(374));
}

#[test]
fn every_setting_the_map_names_comes_back_with_its_value() {
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    let snapshot = SettingsSnapshot::from_frame(&frame).expect("it decodes");

    // Checked against the same capture read a second way, through the register map rather than by offset.
    assert_eq!(value(&snapshot, 250), Some(100), "charge_limit_upper");
    assert_eq!(value(&snapshot, 251), Some(5), "charge_limit_lower");
    assert_eq!(value(&snapshot, 257), Some(200), "slot1_output_power");
    assert_eq!(value(&snapshot, 322), Some(50), "default_output_power");
    assert_eq!(value(&snapshot, 326), Some(0), "grid_power_allowed");
}

#[test]
fn a_register_the_map_does_not_name_is_left_out() {
    // The declared range is a union of per-component banks with gaps between them, so being inside it is
    // not evidence that a register means anything. Taking every number in range would invent settings.
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    let snapshot = SettingsSnapshot::from_frame(&frame).expect("it decodes");

    for (register, _) in &snapshot.values {
        assert!(
            HoldingRegister::lookup(*register).is_some(),
            "{register} is not in the holding map and should not have been decoded"
        );
    }
    // 260 is inside 250..=374 and is the second slot's start time, which one slot's catalogue omits —
    // proving the filter is the map rather than the range.
    assert!(snapshot.end > Register(260));
    assert_eq!(value(&snapshot, 253), None, "253 is a gap in the map");
}

#[test]
fn the_values_are_in_register_order() {
    // So a consumer can merge them into a cache without sorting, and so a duplicate would be obvious.
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    let snapshot = SettingsSnapshot::from_frame(&frame).expect("it decodes");

    let mut previous: Option<Register> = None;
    for (register, _) in &snapshot.values {
        if let Some(last) = previous {
            assert!(*register > last, "{register} follows {last}");
        }
        previous = Some(*register);
    }
    assert!(snapshot.values.len() > 10, "the snapshot carries the settings space");
}

#[test]
fn the_slot_block_decodes_as_the_schedule_it_is() {
    // Five registers per slot from 254, which is the arithmetic the map does — worth pinning here because
    // an off-by-one in the block offset would still produce plausible numbers.
    let frame = Frame::parse(SNAPSHOT).expect("the fixture parses");
    let snapshot = SettingsSnapshot::from_frame(&frame).expect("it decodes");

    let slot1 = HoldingRegister::slot(1).expect("slot 1 exists");
    for entry in slot1 {
        let raw = value(&snapshot, entry.register.number()).map(Raw);
        assert!(raw.is_some(), "{} is missing from the snapshot", entry.name);
    }
    assert_eq!(value(&snapshot, 256), Some(0), "slot1_work_mode was load_first");
    assert_eq!(value(&snapshot, 258), Some(0), "slot1_enabled");
}
