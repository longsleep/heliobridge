//! The Growatt protocol family.
//!
//! This module is deliberately free of any MQTT or Home Assistant types. It is a pure
//! `bytes → values` and `values → bytes` library with no I/O, which is what makes it testable
//! against recorded frames.
//!
//! # Why there is a version namespace
//!
//! Octets 2–3 of every frame are a **protocol generation** selector, not a constant. Only generation
//! `7` was observed on the device this was written against, and only generation 7 is implemented —
//! but the field exists because the family has more than one generation, and older Growatt
//! dataloggers are known to use lower numbers with different body layouts and different obfuscation.
//!
//! So the split is:
//!
//! - [`header`] — the 8-octet frame header. Its layout is what lets a receiver discover the
//!   generation before it can know how to parse anything else, so it must be generation-agnostic.
//! - [`v7`] — everything specific to generation 7: obfuscation, integrity, body layouts, register
//!   maps.
//!
//! Adding a generation means adding a sibling of [`v7`] and a match arm in [`Codec::for_version`].
//! Nothing above this module needs to change, because the vendor-neutral types a decoder produces
//! live in [`crate::model`].
//!
//! # The one part that is not pure decoding
//!
//! [`cloud`] is the optional relay to Growatt's own servers. It speaks MQTT and does I/O, which the rest
//! of this module deliberately does not — but the endpoint, the credentials and the topics are all
//! Growatt's, so it belongs to the vendor rather than to the transport. Everything below [`v7`] stays a
//! pure `bytes → values` library.
//!
//! # Adding a different vendor
//!
//! A different vendor's protocol would be a sibling of this module — `src/<vendor>.rs` — producing
//! the same [`crate::model::Reading`] values. It would not reuse [`header`], because a frame header
//! is exactly the part that is vendor-specific. What it would share is the data model, which is the
//! part worth sharing.
//!
//! No such abstraction is imposed yet. There is one implementation, and a trait with a single
//! implementor is a guess about the second one. [`Codec`] exists only because version dispatch is a
//! real, observed requirement.

pub mod cloud;
pub mod driver;
pub mod firmware;
pub mod header;
pub mod policy;
pub mod product;
pub mod v7;

use core::fmt;

/// A protocol generation, from octets 2–3 of the frame header.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// Generation 7: the one this device speaks, and the only one implemented.
    pub const V7: Self = Self(7);

    /// The raw generation number.
    pub const fn number(self) -> u16 {
        self.0
    }

    /// Whether a codec exists for this generation.
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::V7)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Which generation's codec to use.
///
/// Deliberately a plain enum rather than a boxed trait object. There is one variant; dispatch is a
/// `match`, and adding a generation makes the compiler name every site that needs a decision. That
/// is the property worth having here — a trait object would let a new generation compile while
/// silently falling through somewhere.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Generation 7.
    V7,
}

impl Codec {
    /// Select a codec for a generation, or `None` if none is implemented.
    pub const fn for_version(version: ProtocolVersion) -> Option<Self> {
        match version {
            ProtocolVersion::V7 => Some(Self::V7),
            _ => None,
        }
    }

    /// The generation this codec implements.
    pub const fn version(self) -> ProtocolVersion {
        match self {
            Self::V7 => ProtocolVersion::V7,
        }
    }
}

/// Read the protocol generation from a frame without committing to parsing it.
///
/// This is how a receiver decides which codec to hand the octets to. Returns `None` if there are not
/// even enough octets for a header.
pub fn peek_version(wire: &[u8]) -> Option<ProtocolVersion> {
    header::Header::peek(wire).map(|h| h.protocol)
}

#[cfg(test)]
mod tests {
    use super::{Codec, ProtocolVersion, peek_version};

    #[test]
    fn version_seven_is_supported() {
        assert!(ProtocolVersion::V7.is_supported());
        assert_eq!(Codec::for_version(ProtocolVersion::V7), Some(Codec::V7));
        assert_eq!(Codec::V7.version(), ProtocolVersion::V7);
    }

    #[test]
    fn other_generations_are_recognised_but_unsupported() {
        // The point of the version field: a generation we cannot parse must be distinguishable from
        // a corrupt frame, so it can be logged as "unsupported" rather than "malformed".
        for n in [5, 6, 8] {
            let version = ProtocolVersion(n);
            assert!(!version.is_supported(), "v{n} should not claim support");
            assert_eq!(Codec::for_version(version), None);
        }
    }

    #[test]
    fn version_is_readable_before_parsing() {
        let mut wire = [0u8; 40];
        wire[3] = 7;
        assert_eq!(peek_version(&wire), Some(ProtocolVersion::V7));
        wire[3] = 6;
        assert_eq!(peek_version(&wire), Some(ProtocolVersion(6)));
        assert_eq!(peek_version(&[0, 1]), None);
    }

    #[test]
    fn version_displays_compactly() {
        assert_eq!(ProtocolVersion::V7.to_string(), "v7");
    }
}
