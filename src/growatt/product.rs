//! Which product a device is.
//!
//! The datalogger says so itself: configuration key 13, `device_type`, carries a numeric **device type
//! code** in its identity report. The same code appears in the Bluetooth advertisement, where the vendor's
//! app reads it to label a device in its scan list — so the codes and their names come from the vendor
//! rather than from this program.
//!
//! # Why not the serial
//!
//! A serial's leading characters do identify a product, and they arrive earlier — in the MQTT CONNECT,
//! ahead of any decoding. But the mapping from prefix to product lives only in Growatt's cloud. Neither
//! the datalogger firmware nor the vendor's app carries a table, because neither needs one: the firmware
//! never reads its own serial and the app resolves a product from this type code or from a cloud device
//! record. A prefix table can therefore only be filled one entry at a time, from a serial belonging to a
//! device somebody owns, while the type code names every product the firmware serves.
//!
//! The cost is timing. The identity report arrives about five seconds into a session, so a device is
//! [`Product::Unrecognised`] until it does — which is the same state as a product this build has not been
//! taught, and callers already have to handle it.
//!
//! # Why this matters beyond a display name
//!
//! One firmware serves several products: the image a NEXA is offered names itself `noah2000`, and the
//! firmware's compiled-in default for this very field is the NOAH's code. So the transport, framing,
//! integrity check and function codes are shared, and the **settings registers agree across the family** —
//! but the telemetry register maps do not. A register that reports a heater on one product reports
//! household load on another.
//!
//! This type is therefore the hook a per-product telemetry map would hang from. It does not select one
//! yet: only the NEXA map exists, and inventing a second from a third-party table without hardware to
//! check it against would be a worse outcome than a wrong label. What it does today is stop the device
//! page claiming to be something it is not, and make the product visible in a log.
//!
//! # On unrecognised codes
//!
//! An unknown code is [`Product::Unrecognised`], never an error and never a refusal. This bridge is useful
//! to someone whose product postdates this list, and a code it has not been taught is not a reason to
//! leave a device unserved — the frames decode the same way regardless. Callers present it as the vendor
//! name alone.

/// Declare the product table once, and generate everything that has to agree with it.
///
/// The variants, both lookups, the codes and the display names all come from a single declaration. A
/// hand-written table alongside the enum is the thing that drifts — a variant added without a table entry
/// compiles and is then simply never matched — and there is no way to iterate an enum's variants in Rust
/// without either a macro or a dependency. This is the macro.
macro_rules! products {
    ($( $(#[$doc:meta])* $variant:ident = $code:literal => $name:literal ),+ $(,)?) => {
        /// A product a device type code can identify.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub enum Product {
            $( $(#[$doc])* $variant, )+
            /// A device whose type code this build has not been taught, or which has not reported one yet.
            #[default]
            Unrecognised,
        }

        impl Product {
            /// Every variant a type code can identify — all of them but [`Self::Unrecognised`].
            ///
            /// The lookup is a match rather than a search, so nothing in the program walks this. It exists
            /// so a test can, and check the generated table against itself.
            #[cfg(test)]
            const IDENTIFIABLE: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// Identify a product from a device type code.
            pub const fn from_type_code(code: u16) -> Self {
                match code {
                    $( $code => Self::$variant, )+
                    _ => Self::Unrecognised,
                }
            }

            /// The device type code this product reports. The inverse of the lookup, for the same test.
            #[cfg(test)]
            const fn type_code(self) -> Option<u16> {
                match self {
                    $( Self::$variant => Some($code), )+
                    Self::Unrecognised => None,
                }
            }

            /// The product name, as the vendor writes it, or `None` when the code is unrecognised.
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
    /// NOAH 2000 — the product this datalogger firmware names itself after.
    Noah2000 = 61 => "NOAH 2000",
    /// NEXA 2000 — the product this bridge was written against.
    Nexa2000 = 72 => "NEXA 2000",
    /// AURA 5000, which shares its code with the NODE 5000.
    Aura5000 = 73 => "AURA/NODE 5000",
    /// VETA 2200.
    Veta2200 = 83 => "VETA 2200",
}

impl Product {
    /// Identify a product from an identity report's `device_type` value.
    ///
    /// The value is decimal text, as every configuration value is, and `None` is a report that has not
    /// arrived or one carrying no such field. A value that is not a number this build knows gives
    /// [`Self::Unrecognised`], the same as no value at all: there is nothing to say about either.
    pub fn reported(device_type: Option<&str>) -> Self {
        device_type
            .and_then(|value| value.trim().parse().ok())
            .map_or(Self::Unrecognised, Self::from_type_code)
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
    fn known_codes_are_identified() {
        // 72 is what this device reports, and what its Bluetooth advertisement carries.
        assert_eq!(Product::from_type_code(72), Product::Nexa2000);
        assert_eq!(Product::from_type_code(61), Product::Noah2000);
    }

    #[test]
    fn an_unknown_code_is_unrecognised_rather_than_an_error() {
        assert_eq!(Product::from_type_code(0), Product::Unrecognised);
        assert_eq!(Product::from_type_code(66), Product::Unrecognised);
        assert_eq!(Product::from_type_code(u16::MAX), Product::Unrecognised);
    }

    #[test]
    fn every_identifiable_product_round_trips() {
        // The macro makes drift impossible, so this is no longer guarding a hand-written list — it checks
        // the generated table is actually reachable through the lookup.
        for product in Product::IDENTIFIABLE.iter().copied() {
            let code = product.type_code().expect("an identifiable product carries a code");
            assert_eq!(
                Product::from_type_code(code),
                product,
                "{code} should identify {product}"
            );
        }
    }

    #[test]
    fn the_unrecognised_variant_carries_no_code() {
        assert_eq!(Product::Unrecognised.type_code(), None);
    }

    #[test]
    fn a_reported_value_is_parsed_as_decimal_text() {
        assert_eq!(Product::reported(Some("72")), Product::Nexa2000);
        // Configuration values arrive as text of unpromised shape, so surrounding space is not a reason to
        // fail to identify a device.
        assert_eq!(Product::reported(Some(" 72 ")), Product::Nexa2000);
    }

    #[test]
    fn a_missing_or_unusable_value_is_unrecognised() {
        // No report yet, a report without the field, and a field this build cannot make sense of all leave
        // the product unknown — which is one state, not three.
        assert_eq!(Product::reported(None), Product::Unrecognised);
        assert_eq!(Product::reported(Some("")), Product::Unrecognised);
        assert_eq!(Product::reported(Some("NEXA")), Product::Unrecognised);
        assert_eq!(Product::reported(Some("-1")), Product::Unrecognised);
    }

    #[test]
    fn only_the_written_for_product_claims_a_matching_map() {
        assert!(Product::Nexa2000.telemetry_map_matches());
        assert!(!Product::Noah2000.telemetry_map_matches());
        assert!(!Product::Aura5000.telemetry_map_matches());
        assert!(!Product::Unrecognised.telemetry_map_matches());
    }

    #[test]
    fn display_falls_back_without_panicking() {
        assert_eq!(Product::Veta2200.to_string(), "VETA 2200");
        assert_eq!(Product::Unrecognised.to_string(), "unrecognised product");
    }
}
