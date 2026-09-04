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
//! The seam is split by capability — [`Wire`] for framing, [`Firmware`] for update campaigns, and more to
//! come — rather than gathered into a single wide trait. A consumer bounds on the capability it uses and
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
//! Only framing and firmware exist so far. Telemetry, settings, the register maps, the policy for cloud
//! writes and the cloud relay itself still reach the server as Growatt types directly. This module is the
//! direction of travel and the place the next capability goes, not a finished abstraction layer, and it
//! says so rather than implying more separation than exists.

pub mod firmware;
pub mod wire;

pub use firmware::{AdvertisedFirmware, Firmware};
pub use wire::Wire;

/// Everything the server asks of a driver.
///
/// A bundle rather than a trait in its own right: it has no methods, and a blanket impl gives it to any
/// type implementing every capability. Bound on this where a whole driver is meant — a session — and on a
/// single capability everywhere else.
pub trait Driver: Wire + Firmware {}

impl<T> Driver for T where T: Wire + Firmware {}

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

    fn parse<'a>(&self, _payload: &'a [u8]) -> Option<Self::Frame<'a>> {
        None
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
