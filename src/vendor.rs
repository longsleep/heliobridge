//! The seam between this server and a vendor's way of doing things.
//!
//! Everything specific to Growatt lives under [`crate::growatt`]. This module is where the rest of the
//! program says what it needs from a vendor, in its own vocabulary, so that the two can be reasoned about
//! separately: the server owns *what happens*, a vendor implementation owns *what the bytes mean*.
//!
//! # Why the trait lives here rather than beside the implementation
//!
//! A trait defined next to its only implementation documents that implementation; a trait defined next to
//! its *caller* constrains it. These are the caller's requirements — the abstraction is owned by the side
//! that depends on it, so adding a second vendor cannot quietly widen what the server is allowed to assume,
//! and a change in a vendor's protocol cannot reach the server except through a signature here.
//!
//! # Parse, then interpret
//!
//! [`Vendor::parse`] turns octets into [`Vendor::Message`] — a type the vendor chooses and the server never
//! names. Everything else takes that message back. So a vendor implementation works on its own strongly
//! typed values rather than re-reading bytes for every question, while the server holds something it can
//! pass along and nothing it can misread. The cost is an associated type, which makes this trait generic
//! rather than object-safe: consumers are generic over the vendor, and the vendor is chosen once, where the
//! program is composed.
//!
//! # One assumption remains
//!
//! [`Vendor::firmware_request`] returns an HTTP request, so the seam assumes firmware is fetched over HTTP.
//! That is the server's own transport rather than a vendor's protocol, and naming it here is better than
//! pretending otherwise; a vendor distributing firmware another way would need the seam widened, and would
//! deserve to.
//!
//! # Scope, honestly stated
//!
//! Only the firmware capability exists so far. Telemetry, settings, framing and the identity report still
//! reach the server as Growatt types directly, and moving them is a larger job than adding a method — the
//! register maps in particular are a vendor's model of the device rather than a detail behind an interface.
//! This module is the direction of travel and the place the next capability goes, not a finished
//! abstraction layer, and it says so rather than implying more separation than exists.

use url::Url;

/// A firmware image a vendor's cloud has advertised as installable.
///
/// Vendor-neutral by construction: somewhere to fetch it from, a name to keep it under, and a phrase saying
/// where the advertisement was found so that a log line can be specific without the reader — or this
/// program — knowing what a configuration register is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedFirmware {
    /// Where the image can be fetched.
    pub url: Url,
    /// What to call it on disk, before any sanitising the filesystem needs.
    pub file: String,
    /// Where it was advertised, in the vendor's own terms, for logging only.
    pub source: String,
}

/// One vendor's protocol, as much of it as the server currently needs.
pub trait Vendor: std::fmt::Debug + Send + Sync + 'static {
    /// A message from the cloud, in whatever form this vendor's protocol gives it.
    ///
    /// Opaque to the server, which only ever obtains one from [`Self::parse`] and hands it back. That is
    /// what lets an implementation be strongly typed in its own terms — a parsed frame, a decoded
    /// envelope, an enumeration of message kinds — without any of it reaching this side of the seam.
    type Message<'a>;

    /// Read octets as a message, or `None` if they are not one.
    ///
    /// Octets are the only input the server can honestly offer: every transport delivers bytes, and
    /// anything more structured would be a shape borrowed from one vendor's protocol. A payload that does
    /// not parse is not an error — this program stands between a device and a cloud it does not control,
    /// and a message it cannot read is a fact about the world rather than a fault.
    fn parse<'a>(&self, payload: &'a [u8]) -> Option<Self::Message<'a>>;

    /// Firmware this message advertises, if it advertises any.
    fn advertised_firmware(&self, message: &Self::Message<'_>) -> Option<AdvertisedFirmware>;

    /// A request for an advertised image, shaped the way this vendor's own device would send it.
    ///
    /// Returns a builder rather than a request: the body type belongs to whichever client performs the
    /// transfer, and there is none for a `GET`. Every header the vendor's device sends **must** be set
    /// here, because the caller adds none of its own.
    fn firmware_request(&self, firmware: &AdvertisedFirmware) -> http::request::Builder;
}

/// A vendor that recognises nothing.
///
/// The null implementation, for wiring that has no vendor selected and for tests about anything other than
/// a vendor. It is not a fallback: a deployment serving a real device chooses a real implementation, and
/// this one would quietly notice nothing at all — which is exactly what makes it useful in a test that
/// would otherwise have to pick a vendor it does not care about.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unknown;

impl Vendor for Unknown {
    /// Nothing parses, so there is nothing to describe.
    type Message<'a> = ();

    fn parse<'a>(&self, _payload: &'a [u8]) -> Option<Self::Message<'a>> {
        None
    }

    fn advertised_firmware(&self, _message: &Self::Message<'_>) -> Option<AdvertisedFirmware> {
        None
    }

    fn firmware_request(&self, _firmware: &AdvertisedFirmware) -> http::request::Builder {
        http::Request::builder()
    }
}
