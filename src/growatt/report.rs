//! Which message type says what, and what it decodes to.
//!
//! The dispatch that used to sit in the session: one arm per generation-7 message type, each decoding the
//! body it knows and telling a [`Sink`]. Keeping it here means the server never learns that a `0x04` is
//! telemetry and a `0x19` is the hourly settings snapshot, and a second generation is a second table
//! rather than an edit to the session.

use crate::driver::report::{Field, Identity, Sink, Snapshot, Telemetry, WriteAck};
use crate::growatt::v7::decode::{FromFrame, ReadResponse, SettingsSnapshot};
use crate::growatt::v7::decode::{Telemetry as DecodedTelemetry, WriteAck as DecodedAck};
use crate::growatt::v7::frame::{Frame, MessageType};
use crate::growatt::v7::identity::Identity as DecodedIdentity;
use crate::growatt::v7::registers::Role;

/// Read one frame and tell `to` what it says.
pub fn report(frame: &Frame, to: &mut dyn Sink) {
    let kind = frame.message_type();
    match kind {
        MessageType::Telemetry | MessageType::BufferedTelemetry => {
            // Buffered records are samples the device took earlier and held until it could reach a server
            // — observed 68 minutes stale — so whoever is told has to be able to tell them apart.
            let buffered = matches!(kind, MessageType::BufferedTelemetry);
            match DecodedTelemetry::from_frame(frame) {
                Ok(telemetry) => to.telemetry(&Telemetry {
                    at: telemetry.timestamp,
                    readings: &telemetry.readings,
                    buffered,
                }),
                Err(error) => to.unreadable(&kind.to_string(), &error),
            }
        }

        MessageType::ReadSingleRegister => match ReadResponse::from_frame(frame) {
            Ok(response) => to.read_answer(response.register, response.raw),
            Err(error) => to.unreadable(&kind.to_string(), &error),
        },

        MessageType::WriteSingleRegister | MessageType::WriteRegisterRange => match DecodedAck::from_frame(frame) {
            Ok(ack) => to.write_ack(&WriteAck {
                first: ack.start,
                last: ack.end,
                accepted: ack.accepted(),
                value: ack.value,
                status: format!("{:#04x}", ack.status),
            }),
            Err(error) => to.unreadable(&kind.to_string(), &error),
        },

        // The unsolicited report and the answer to a config read share a body layout: one carries
        // everything the device knows about itself, the other the single register asked for.
        MessageType::IdentityReport | MessageType::ConfigRead => match DecodedIdentity::from_frame(frame) {
            Ok(identity) => to.identity(&Identity {
                declared: identity.declared,
                truncated: identity.truncated,
                endpoint: identity.endpoint(),
                summary: identity.summary(),
                fields: identity
                    .entries
                    .iter()
                    .map(|entry| Field {
                        register: entry.register,
                        name: entry.name(),
                        role: entry.role().map(Role::as_str),
                        value: &entry.value,
                    })
                    .collect(),
            }),
            Err(error) => to.unreadable(&kind.to_string(), &error),
        },

        MessageType::SettingsSnapshot => match SettingsSnapshot::from_frame(frame) {
            Ok(snapshot) => to.snapshot(&Snapshot {
                first: snapshot.start,
                last: snapshot.end,
                values: &snapshot.values,
            }),
            Err(error) => to.unreadable(&kind.to_string(), &error),
        },

        MessageType::ConfigWrite => to.undecoded(&kind.to_string(), frame.wire_len()),

        MessageType::Unrecognised { address, function } => to.unknown(
            &format!("address {address:#04x} function {function:#04x}"),
            frame.wire_len(),
        ),
    }
}
