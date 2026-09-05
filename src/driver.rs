//! The seam between this server and one manufacturer's way of doing things.
//!
//! Everything specific to Growatt lives under [`crate::growatt`]. This module is where the rest of the
//! program says what it needs from a driver, in its own vocabulary, so that the two can be reasoned about
//! separately: the server owns *what happens*, a driver owns *what the bytes mean*.
//!
//! # Why the traits live here rather than beside the implementation
//!
//! A trait defined next to its only implementation documents that implementation; a trait defined next to
//! its *caller* constrains it. These are the caller's requirements — the abstraction is owned by the side
//! that depends on it, so adding a second driver cannot quietly widen what the server is allowed to assume,
//! and a change in a manufacturer's protocol cannot reach the server except through a signature here.
//!
//! # One capability per trait, one bundle to name them
//!
//! The seam is split by capability — [`Wire`] for framing, [`Report`] for what a frame says, [`Arbiter`]
//! for what it is trying to do, [`Catalogue`] for what the device holds, [`Commands`] for asking it to do
//! something,
//! [`Firmware`] for update campaigns, [`Upstream`] for the manufacturer's cloud, and more to come — rather than gathered into a single wide trait. A consumer bounds on the capability it uses and
//! nothing else, which keeps its requirements legible: `server::firmware` needs [`Firmware`], and the fact
//! that it needs nothing else is worth being able to see.
//!
//! [`Driver`] then bundles every capability for the places that need a whole driver rather than one
//! aspect of one — a session, which does all of it. It is a bundle and not a trait with methods of its
//! own: it is implemented by a blanket impl, so a driver is complete by implementing the capabilities and
//! there is nothing extra to remember.
//!
//! # Parse, then interpret
//!
//! [`Wire::parse`] turns octets into [`Wire::Frame`] — a type the driver chooses and the server never
//! names. Every other capability takes that frame back. So a driver works on its own strongly typed values
//! rather than re-reading bytes for every question, while the server holds something it can pass along and
//! nothing it can misread. The cost is an associated type, which makes these traits generic rather than
//! object-safe: consumers are generic over the driver, and the driver is chosen once, where the program is
//! composed.
//!
//! # Scope, honestly stated
//!
//! Framing, firmware and the cloud relay exist so far. Telemetry, settings, the register maps and the
//! policy for cloud writes still reach the server as Growatt types directly. This module is the
//! direction of travel and the place the next capability goes, not a finished abstraction layer, and it
//! says so rather than implying more separation than exists.

pub mod arbiter;
pub mod catalogue;
pub mod commands;
pub mod firmware;
pub mod report;
pub mod upstream;
pub mod wire;

pub use arbiter::{Arbiter, Direction, Intent, Policy};
pub use catalogue::{Catalogue, ConfigField, Measurement, Setting, Shape};
pub use commands::{Command, Commands, Outgoing};
pub use firmware::{AdvertisedFirmware, Firmware};
pub use report::{Report, Sink};
pub use upstream::{Endpoint, Message, Relay, Target, Upstream};
pub use wire::{Unreadable, Wire};

use crate::model::Register;

/// Everything the server asks of a driver.
///
/// A bundle rather than a trait in its own right: it has no methods, and a blanket impl gives it to any
/// type implementing every capability. Bound on this where a whole driver is meant — a session — and on a
/// single capability everywhere else.
pub trait Driver: Wire + Arbiter + Catalogue + Commands + Firmware + Report + Upstream {}

impl<T> Driver for T where T: Wire + Arbiter + Catalogue + Commands + Firmware + Report + Upstream {}

/// A driver that recognises nothing.
///
/// The null implementation, for wiring that has no driver selected and for tests about anything other than
/// a driver. It is not a fallback: a deployment serving a real device chooses a real implementation, and
/// this one would quietly notice nothing at all — which is exactly what makes it useful in a test that
/// would otherwise have to pick a driver it does not care about.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unknown;

impl Wire for Unknown {
    /// Nothing parses, so there is nothing to describe.
    type Frame<'a> = ();

    fn read<'a>(&self, _payload: &'a [u8]) -> Result<Self::Frame<'a>, Unreadable> {
        Err(Unreadable::Unsupported {
            generation: "none: this driver reads nothing".to_owned(),
        })
    }
}

impl Catalogue for Unknown {
    type Setting = catalogue::Undocumented;
    type Measurement = catalogue::Undocumented;
    type ConfigField = catalogue::Undocumented;

    fn settings(&self, _slots: u16) -> Vec<Self::Setting> {
        Vec::new()
    }

    fn setting(&self, _register: Register) -> Option<Self::Setting> {
        None
    }

    fn setting_named(&self, _name: &str) -> Option<Self::Setting> {
        None
    }

    fn describe(&self, register: Register) -> Self::Setting {
        catalogue::Undocumented(register)
    }

    fn slot(&self, _slot: u16) -> Option<Vec<Self::Setting>> {
        None
    }

    fn slots(&self) -> u16 {
        0
    }

    fn measurements(&self) -> Vec<Self::Measurement> {
        Vec::new()
    }

    fn config(&self, _register: Register) -> Option<Self::ConfigField> {
        None
    }

    fn config_named(&self, _name: &str) -> Option<Self::ConfigField> {
        None
    }

    fn config_last(&self) -> Register {
        Register(0)
    }

    fn writable_config(&self) -> Vec<Self::ConfigField> {
        Vec::new()
    }
}

impl Commands for Unknown {
    type Error = commands::Unsupported;

    fn prepare(&self, _device_id: &str, _command: &Command) -> Result<Outgoing, Self::Error> {
        Err(commands::Unsupported)
    }
}

impl Report for Unknown {
    /// Unreachable in practice: nothing parses, so there is never a frame to report on.
    fn report(&self, _frame: &Self::Frame<'_>, _to: &mut dyn Sink) {}
}

impl Arbiter for Unknown {
    /// Nothing is recognised, which the downlink policy refuses — the safe answer for a driver that cannot
    /// read a frame in the first place.
    fn intent(&self, _frame: &Self::Frame<'_>, _direction: Direction) -> Intent {
        Intent::Unrecognised
    }
}

impl Upstream for Unknown {
    type Relay = upstream::NoRelay;
    type Error = upstream::NoUpstream;

    /// Nowhere: a driver that recognises nothing knows no cloud to point at either.
    fn endpoint(&self) -> Endpoint {
        Endpoint {
            host: String::new(),
            port: 0,
        }
    }

    fn certificate_names(&self) -> &'static [&'static str] {
        &[]
    }

    fn relay(&self, _device_id: &str, _target: Target) -> Result<Self::Relay, Self::Error> {
        Err(upstream::NoUpstream)
    }
}

impl Firmware for Unknown {
    fn advertised(&self, _frame: &Self::Frame<'_>) -> Option<AdvertisedFirmware> {
        None
    }

    fn request(&self, _firmware: &AdvertisedFirmware) -> http::request::Builder {
        http::Request::builder()
    }
}
