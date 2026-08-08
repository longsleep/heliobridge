//! Recognising what a frame is trying to do, for the relay policy.
//!
//! The vocabulary is [`crate::growatt::policy::Intent`], which is vendor- and generation-neutral; this module is
//! the generation-7 half of the translation. A new generation supplies its own [`Frame::intent`] and the
//! policy rules stay untouched.
//!
//! # Direction decides meaning
//!
//! Three message types mean different things depending on who sent them. `0x05` is a request downstream
//! and an answer upstream; `0x06` and `0x10` are writes downstream and acknowledgements upstream — with
//! the register fields at the same offsets either way, which is why one classifier handles both.
//!
//! # A truncated body is not an intent
//!
//! When a register cannot be read from where it should be, the result is
//! [`Intent::Unrecognised`] rather than a guess. The downlink policy refuses that, which is the safe
//! direction to be wrong in.

use crate::growatt::policy::{Direction, Intent};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::model::Register;

impl Frame {
    /// What this frame is trying to do, given the direction it is travelling.
    pub fn intent(&self, direction: Direction) -> Intent {
        let to_device = matches!(direction, Direction::ToDevice);
        match self.message_type() {
            MessageType::Telemetry | MessageType::ExtendedTelemetry => Intent::Telemetry,
            MessageType::SettingsSnapshot => Intent::SettingsSnapshot,
            MessageType::IdentityReport => Intent::Identity,

            // The config write carries its register inside the first TLV entry: count, entry length,
            // then the register.
            MessageType::ConfigWrite => match self.register_at(4) {
                Some(register) => Intent::WriteConfig { register },
                None => Intent::Unrecognised,
            },

            MessageType::ReadSingleRegister => match self.register_at(0) {
                Some(register) if to_device => Intent::ReadRequest { register },
                Some(register) => Intent::ReadResponse { register },
                None => Intent::Unrecognised,
            },

            MessageType::WriteSingleRegister => match self.register_at(0) {
                Some(register) => Self::write_intent(to_device, register, register),
                None => Intent::Unrecognised,
            },

            MessageType::WriteRegisterRange => match (self.register_at(0), self.register_at(2)) {
                (Some(start), Some(end)) => Self::write_intent(to_device, start, end),
                _ => Intent::Unrecognised,
            },

            MessageType::Unrecognised { .. } => Intent::Unrecognised,
        }
    }

    /// A write travelling to the device, or the acknowledgement of one coming back.
    const fn write_intent(to_device: bool, start: Register, end: Register) -> Intent {
        if to_device {
            Intent::WriteSettings { start, end }
        } else {
            Intent::WriteAck { start, end }
        }
    }

    /// A big-endian register number at `offset` in the body, or `None` if the body is too short.
    fn register_at(&self, offset: usize) -> Option<Register> {
        let body = self.body();
        let end = offset.checked_add(2)?;
        let pair = body.get(offset..end)?;
        Some(Register(u16::from_be_bytes(<[u8; 2]>::try_from(pair).ok()?)))
    }
}

#[cfg(test)]
mod tests {
    use crate::growatt::policy::{Direction, Intent};
    use crate::growatt::v7::encode::Command;
    use crate::growatt::v7::frame::Frame;
    use crate::model::Register;

    const SERIAL: &str = "0EXAMPLE00000001";

    /// Build a frame the way the server would, then read it back as the wire would deliver it.
    fn round_trip(command: &Command) -> Frame {
        let wire = command.to_frame(SERIAL).expect("build").to_wire();
        Frame::parse(&wire).expect("parse")
    }

    #[test]
    fn a_single_register_write_is_a_settings_write_downstream() {
        let command = Command::set(Register(257), 100).expect("allowed");
        let frame = round_trip(&command);
        assert_eq!(
            frame.intent(Direction::ToDevice),
            Intent::WriteSettings {
                start: Register(257),
                end: Register(257),
            }
        );
    }

    #[test]
    fn the_same_octets_upstream_are_an_acknowledgement() {
        let command = Command::set(Register(257), 100).expect("allowed");
        let frame = round_trip(&command);
        assert_eq!(
            frame.intent(Direction::ToCloud),
            Intent::WriteAck {
                start: Register(257),
                end: Register(257),
            },
            "direction is what distinguishes a write from its acknowledgement"
        );
    }

    #[test]
    fn the_composite_write_is_recognised_as_a_range() {
        // 322 is written as 321..322, which the policy must see as a settings write, not a config one.
        let command = Command::set(Register(322), 100).expect("allowed");
        let frame = round_trip(&command);
        assert_eq!(
            frame.intent(Direction::ToDevice),
            Intent::WriteSettings {
                start: Register(321),
                end: Register(322),
            }
        );
    }

    #[test]
    fn the_time_push_is_a_config_write_to_register_31() {
        let time = crate::growatt::v7::decode::Timestamp {
            year: 2026,
            month: 8,
            day: 8,
            hour: 2,
            minute: 10,
            second: 29,
        };
        let frame = round_trip(&Command::time_push(time).expect("plausible"));
        assert_eq!(
            frame.intent(Direction::ToDevice),
            Intent::WriteConfig { register: Register(31) },
            "the clock is a configuration register, which is what lets the policy refuse it"
        );
    }

    #[test]
    fn a_read_is_a_request_downstream_and_an_answer_upstream() {
        let frame = round_trip(&Command::read(Register(250)));
        assert_eq!(
            frame.intent(Direction::ToDevice),
            Intent::ReadRequest {
                register: Register(250)
            }
        );
        assert_eq!(
            frame.intent(Direction::ToCloud),
            Intent::ReadResponse {
                register: Register(250)
            }
        );
    }

    #[test]
    fn telemetry_and_snapshots_are_named_from_the_fixtures() {
        let cases = [
            ("battery-discharging-soc-33.bin", Intent::Telemetry),
            ("settings-snapshot.bin", Intent::SettingsSnapshot),
        ];
        for (name, expected) in cases {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
            let Ok(wire) = std::fs::read(format!("{path}{name}")) else {
                continue;
            };
            let frame = Frame::parse(&wire).expect("fixture parses");
            assert_eq!(frame.intent(Direction::ToCloud), expected, "{name}");
        }
    }
}
