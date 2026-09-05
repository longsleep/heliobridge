//! Which product this is, and what it is running.
//!
//! Both are things only a driver can say, and both exist for the same reason: a device page that names the
//! model and the firmware is the difference between "some inverter" and "the one in the garage".
//!
//! # Answered from what the device reported
//!
//! Nothing here fetches anything. A server already holds the device's own description of itself and its
//! last telemetry frame; these methods turn those into words. A driver looks up the fields it needs by
//! name, because which fields those are is precisely what a server does not know.

use super::wire::Wire;

/// Values a device reported, looked up by the driver's own field names.
///
/// Values arrive as text, including the numeric ones: this is what the device said, and a driver that
/// needs a number knows how its own field is spelt.
pub type Reported<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Naming the product and its firmware.
pub trait Describes: Wire {
    /// What to call the product a device reporting `device_type` is.
    ///
    /// `None` when the driver does not recognise the code — which is a fact worth showing as it is, rather
    /// than papering over with a guess. A server can still show the code itself.
    fn product_name(&self, device_type: Option<&str>) -> Option<&'static str>;

    /// Whether this driver's readings were written for that product.
    ///
    /// A family often shares settings while differing in what its telemetry means, so this is the
    /// difference between "nothing works" and "some labels may be wrong" — worth saying out loud once
    /// rather than leaving a reader to wonder.
    fn telemetry_matches(&self, device_type: Option<&str>) -> bool;

    /// One firmware version, assembled from whatever fields carry its parts.
    ///
    /// A single string because that is what a device page has room for; which registers went into it is
    /// the driver's business.
    fn firmware_version(&self, reported: &Reported<'_>) -> Option<String>;
}
