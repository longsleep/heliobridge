//! Which product a serial identifies.
//!
//! The leading characters of a datalogger serial are a product code, and it is the only identifier
//! available before anything has been decoded — it arrives in the MQTT CONNECT, ahead of the identity
//! report and ahead of the first frame.
//!
//! # Why this matters beyond a display name
//!
//! One firmware serves several products: the image a NEXA is offered names itself `noah2000`. So the
//! transport, framing, integrity check and function codes are shared, and the **settings registers agree
//! across the family** — but the telemetry register maps do not. A register that reports a heater on one
//! product reports household load on another.
//!
//! This type is therefore the hook a per-product telemetry map would hang from. It does not select one
//! yet: only the NEXA map exists, and inventing a second from a third-party table without hardware to
//! check it against would be a worse outcome than a wrong label. What it does today is stop the device
//! page claiming to be something it is not, and make the product visible in a log.
//!
//! # On unrecognised prefixes
//!
//! An unknown prefix is [`Product::Unrecognised`], never an error and never a refusal. This bridge is
//! useful to someone whose product predates this list, and a serial it has not been taught is not a
//! reason to leave a device unserved — the frames decode the same way regardless. Callers present it as
//! the vendor name alone.

/// Declare the product table once, and generate everything that has to agree with it.
///
/// The variants, the list to search, the prefixes and the display names all come from a single
/// declaration. A hand-written list alongside the enum is the thing that drifts — a variant added
/// without a list entry compiles and is then simply never matched — and there is no way to iterate an
/// enum's variants in Rust without either a macro or a dependency. This is the macro.
///
/// Prefix lengths deliberately are not fixed: the family uses three and four characters, so matching is
/// by `starts_with` rather than by slicing a constant width.
macro_rules! products {
    ($( $(#[$doc:meta])* $variant:ident = $prefix:literal => $name:literal ),+ $(,)?) => {
        /// A product a datalogger serial can identify.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub enum Product {
            $( $(#[$doc])* $variant, )+
            /// A serial whose prefix this build has not been taught.
            #[default]
            Unrecognised,
        }

        impl Product {
            /// Every variant a serial can identify — all of them but [`Self::Unrecognised`].
            const IDENTIFIABLE: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The serial prefix that identifies this product.
            const fn prefix(self) -> Option<&'static str> {
                match self {
                    $( Self::$variant => Some($prefix), )+
                    Self::Unrecognised => None,
                }
            }

            /// The product name, as the vendor writes it, or `None` when the prefix is unrecognised.
            pub const fn name(self) -> Option<&'static str> {
                match self {
                    $( Self::$variant => Some($name), )+
                    Self::Unrecognised => None,
                }
            }
        }
    };
}

products! {
    /// NEXA 2000 — the product this bridge was written against.
    Nexa2000 = "0HVR" => "NEXA 2000",
    /// NOAH 2000 — protocol-compatible, with a telemetry register map that differs in places.
    Noah2000 = "0PVP" => "NOAH 2000",
}

impl Product {
    /// Identify a product from a datalogger serial.
    ///
    /// Case-sensitive: observed serials are upper case throughout, and a lower-case prefix would be a
    /// different vendor convention rather than the same one typed differently.
    pub fn from_serial(serial: &str) -> Self {
        Self::IDENTIFIABLE
            .iter()
            .copied()
            .find(|product| product.prefix().is_some_and(|prefix| serial.starts_with(prefix)))
            .unwrap_or_default()
    }

    /// Whether this bridge's telemetry register map was written for this product.
    ///
    /// False does not mean unusable: the settings registers agree across the family, and most telemetry
    /// registers carry the same quantity. It means individual readings may be mislabelled, which is worth
    /// saying once in a log rather than never.
    pub const fn telemetry_map_matches(self) -> bool {
        matches!(self, Self::Nexa2000)
    }
}

impl core::fmt::Display for Product {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name().unwrap_or("unrecognised product"))
    }
}

#[cfg(test)]
mod tests {
    use super::Product;

    #[test]
    fn known_prefixes_are_identified() {
        assert_eq!(Product::from_serial("0HVR000000000001"), Product::Nexa2000);
        assert_eq!(Product::from_serial("0PVP000000000001"), Product::Noah2000);
    }

    #[test]
    fn unknown_prefix_is_unrecognised_rather_than_an_error() {
        assert_eq!(Product::from_serial("QMN000BZP0000000"), Product::Unrecognised);
        assert_eq!(Product::from_serial(""), Product::Unrecognised);
        assert_eq!(Product::from_serial("0HV"), Product::Unrecognised);
    }

    #[test]
    fn every_identifiable_product_round_trips() {
        // The macro makes drift impossible, so this is no longer guarding a hand-written list — it
        // checks the generated table is actually reachable through the prefix search.
        for product in Product::IDENTIFIABLE.iter().copied() {
            let prefix = product.prefix().expect("an identifiable product carries a prefix");
            let serial = format!("{prefix}000000000001");
            assert_eq!(
                Product::from_serial(&serial),
                product,
                "{serial} should identify {product}"
            );
        }
    }

    #[test]
    fn the_unrecognised_variant_carries_no_prefix() {
        assert_eq!(Product::Unrecognised.prefix(), None);
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert_eq!(Product::from_serial("0hvr000000000001"), Product::Unrecognised);
    }

    #[test]
    fn only_the_written_for_product_claims_a_matching_map() {
        assert!(Product::Nexa2000.telemetry_map_matches());
        assert!(!Product::Noah2000.telemetry_map_matches());
        assert!(!Product::Unrecognised.telemetry_map_matches());
    }

    #[test]
    fn display_falls_back_without_panicking() {
        assert_eq!(Product::Noah2000.to_string(), "NOAH 2000");
        assert_eq!(Product::Unrecognised.to_string(), "unrecognised product");
    }
}
