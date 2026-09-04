//! Firmware a manufacturer's cloud advertises as installable.
//!
//! # One assumption is visible here
//!
//! [`Firmware::request`] returns an HTTP request, so this capability assumes firmware is fetched over
//! HTTP. That is the server's own transport rather than a manufacturer's protocol, and naming it here is
//! better than pretending otherwise; a driver distributing firmware another way would need the seam
//! widened, and would deserve to.

use url::Url;

use super::wire::Wire;

/// A firmware image a cloud has advertised as installable.
///
/// Neutral by construction: somewhere to fetch it from, a name to keep it under, and a phrase saying where
/// the advertisement was found so that a log line can be specific without the reader — or this program —
/// knowing what a configuration register is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedFirmware {
    /// Where the image can be fetched.
    pub url: Url,
    /// What to call it on disk, before any sanitising the filesystem needs.
    pub file: String,
    /// Where it was advertised, in the driver's own terms, for logging only.
    pub source: String,
}

/// Recognising and fetching advertised firmware.
pub trait Firmware: Wire {
    /// Firmware this frame advertises, if it advertises any.
    ///
    /// Every frame can be offered: recognising an advertisement is the driver's job, and the common case
    /// is that a frame is not one.
    fn advertised(&self, frame: &Self::Frame<'_>) -> Option<AdvertisedFirmware>;

    /// A request for an advertised image, shaped the way this manufacturer's own device would send it.
    ///
    /// Returns a builder rather than a request: the body type belongs to whichever client performs the
    /// transfer, and there is none for a `GET`. Every header the device sends **must** be set here,
    /// because the caller adds none of its own.
    fn request(&self, firmware: &AdvertisedFirmware) -> http::request::Builder;
}
