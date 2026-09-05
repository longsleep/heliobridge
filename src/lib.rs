//! Heliobridge — a local MQTT bridge for the Growatt Nexa 2000.
//!
//! The device connects here over TLS instead of to the vendor cloud. Heliobridge decodes its
//! protocol, republishes the values to an existing Home Assistant MQTT broker with autodiscovery,
//! accepts commands back, and can optionally relay everything upstream so the vendor app keeps
//! working.
//!
//! # Status
//!
//! Early. The wire protocol is reverse engineered and specified, and the offline codec is
//! implemented; none of the networking exists yet. The API will change without regard for
//! compatibility until 0.1.0.
//!
//! # Layout
//!
//! - [`model`] — vendor-neutral data model: register and value newtypes, units, scaling, readings.
//! - [`driver`] — the seam: what this server asks of a manufacturer's implementation, one trait per
//!   capability. Everything below reaches Growatt through it, and only the binary names Growatt itself.
//! - [`growatt`] — the Growatt protocol family, with a module per protocol generation. Pure
//!   `bytes → values`, no I/O and no MQTT types.
//! - [`mqtt`] — MQTT 3.1.1 packet codec. Transport, and direction-agnostic: the device-facing server
//!   and the vendor cloud client both speak it, which is why it sits here rather than inside either.
//! - [`server`] — everything device-facing: the session state machine, the accept loop, server TLS and
//!   the clock behind the time push.
//! - [`control`] — a local HTTP control API over a Unix socket, off unless configured.
//! - [`record`] — raw frame recording for later analysis, off unless configured.
//! - [`config`] — environment configuration, all of it prefixed `HELIOBRIDGE_`.
//!
//! Still to come: `bridge` (cached state, Home Assistant, optional cloud relay).
//!
//! Module paths double as tracing targets, so that layout is also the logging control surface.

pub mod config;
pub mod control;
pub mod driver;
pub mod growatt;
pub mod homeassistant;
pub mod model;
pub mod mqtt;
pub mod record;
pub mod server;

/// The crate version, as published.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tracing target for full frame hexdumps, in both directions, wherever they are produced.
///
/// A cross-cutting target rather than a module path, because the useful axis is the kind of data
/// and not where it was emitted. Enable with `HELIOBRIDGE_LOG=info,heliobridge::wire=trace`.
pub const TARGET_WIRE: &str = "heliobridge::wire";

/// Tracing target for every decoded register value, each cycle.
///
/// Enable with `HELIOBRIDGE_LOG=info,heliobridge::values=trace`.
pub const TARGET_VALUES: &str = "heliobridge::values";

#[cfg(test)]
mod tests {
    use super::{TARGET_VALUES, TARGET_WIRE, VERSION};

    #[test]
    fn version_is_populated() {
        assert!(!VERSION.is_empty());
    }

    /// The seam holds, or it is not a seam.
    ///
    /// Two modules may name Growatt: the implementation itself, and the binary, which is where a driver is
    /// chosen. Everywhere else reaches it through [`crate::driver`], and the whole value of that is that it
    /// cannot be quietly worked around — an `use crate::growatt::…` added to a handler compiles perfectly
    /// well and undoes the abstraction, so the check has to be mechanical rather than a convention.
    ///
    /// Test modules are exempt and scanning stops at the first `#[cfg(test)]`: a test may well name the
    /// driver it is testing, and this crate keeps its test modules at the foot of the file.
    #[test]
    fn only_the_binary_and_the_implementation_name_a_manufacturer() {
        fn scan(directory: &std::path::Path, offenders: &mut Vec<String>) {
            let entries = std::fs::read_dir(directory).expect("the source tree is readable");
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
                // The implementation, and the one place that chooses it.
                if name.starts_with("growatt") || name == "bin" {
                    continue;
                }
                if path.is_dir() {
                    scan(&path, offenders);
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("a source file is readable");
                for (number, line) in text.lines().enumerate() {
                    if line.contains("#[cfg(test)]") {
                        break;
                    }
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue;
                    }
                    if code.contains("crate::growatt") || code.contains("growatt::") {
                        offenders.push(format!("{}:{}: {}", path.display(), number + 1, code.trim()));
                    }
                }
            }
        }

        let mut offenders = Vec::new();
        scan(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "these reach past the driver seam; ask the driver instead:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn tracing_targets_are_rooted_at_the_crate_name() {
        // EnvFilter matches on a `::`-separated path prefix, so a target that is not rooted at the
        // crate name silently escapes `HELIOBRIDGE_LOG=heliobridge=debug`.
        for target in [TARGET_WIRE, TARGET_VALUES] {
            assert!(
                target.starts_with("heliobridge::"),
                "{target} is not under the crate root"
            );
        }
    }
}
