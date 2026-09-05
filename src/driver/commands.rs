//! Asking a device to do something.
//!
//! # What the server asks for, not how it is spelt
//!
//! A [`Command`] says what should happen — set this setting, read that one, take this meter reading — and
//! a driver turns it into octets. Which registers a setting really occupies, whether a value goes out as
//! one write or five, what the manufacturer's own server puts in each field: none of that is here.
//!
//! # Refusing is part of the job
//!
//! Writing a speculative value to an unknown register on a mains-connected battery inverter is the one
//! action in this program with a real safety warning attached. A driver is therefore expected to refuse a
//! command it cannot express — an unwritable register, a value outside a documented range — and a server
//! must treat [`Commands::prepare`] as the last word rather than a formality. Nothing here can construct
//! octets, so nothing here can get round it.

use crate::model::{Raw, Register, Timestamp};
use crate::mqtt::QoS;

use super::wire::Wire;

/// Something to ask a device to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Store a value in one setting, whatever that takes.
    Set {
        /// The setting.
        register: Register,
        /// The value asked for, unscaled.
        value: u16,
    },

    /// Read one setting back.
    Read {
        /// The setting.
        register: Register,
    },

    /// Read fields of the device's own configuration.
    ///
    /// A list rather than one, because asking for several at once is a single frame where a driver's
    /// protocol allows it.
    ReadConfig {
        /// The fields to ask for.
        registers: Vec<Register>,
    },

    /// Write one field of the device's own configuration.
    ///
    /// Also how an action is asked for: a field the catalogue says is an action is carried out by writing
    /// the trigger value it names. There is no separate variant for restarting, because whether a device
    /// has such a field — and what writing it takes — is exactly what a catalogue is for.
    WriteConfig {
        /// The field.
        register: Register,
        /// The value, as text: configuration fields are text on the wire whatever they mean.
        value: String,
    },

    /// Tell the device what time it is.
    PushTime(Timestamp),

    /// Supply a meter reading the device would otherwise get from its own accessory.
    ///
    /// `valid` false withdraws the supply, which is not the same as supplying zero: a device left with a
    /// stale reading behaves differently from one told there is no meter.
    MeterReading {
        /// Watts, positive for import.
        watts: i32,
        /// Whether the reading is to be believed.
        valid: bool,
    },
}

/// A command, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The octets to publish.
    pub payload: Vec<u8>,
    /// How the manufacturer's own server sends this one, so this program is indistinguishable from it.
    pub qos: QoS,
    /// Whether the device answers it, which decides whether silence means anything.
    pub acknowledged: bool,
    /// What it is, in the driver's words, for one log line.
    pub description: String,
    /// Settings worth reading back afterwards, with the value asked for where there is one.
    ///
    /// Empty when there is nothing to check. A driver may name more registers than the command mentioned:
    /// one setting can move another, and a cached value nobody re-read would then be wrong.
    pub verify: Vec<(Register, Option<Raw>)>,
}

/// Turning a command into something a device will accept.
pub trait Commands: Wire {
    /// Why a command cannot be sent.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Prepare `command` for `device_id`.
    ///
    /// # Errors
    ///
    /// [`Self::Error`] if the driver will not express it: an unwritable register, a value out of range, an
    /// action this protocol has no way to ask for.
    fn prepare(&self, device_id: &str, command: &Command) -> Result<Outgoing, Self::Error>;
}

/// A driver that cannot express commands at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this driver sends no commands")]
pub struct Unsupported;
