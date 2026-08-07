//! The `heliobridge` binary.
//!
//! A placeholder at 0.0.1. Its only job is to exist, report its version, and make it obvious that
//! there is nothing to run yet.
//!
//! This lives in `src/bin/heliobridge/` rather than `src/main.rs` so that a second binary is a
//! second directory, with no `Cargo.toml` change and no restructuring.

// The daemon's output belongs in tracing, which is why `print_stdout` is warned crate-wide. A
// version banner is the one place stdout is the correct destination. `expect` rather than `allow`
// so the exception is removed by the compiler once it stops being true.
#[expect(
    clippy::print_stdout,
    reason = "version banner is intentionally stdout, not a log record"
)]
fn main() {
    println!("heliobridge {}", heliobridge::VERSION);
    println!("Not implemented yet — see the repository for the protocol specification and plan.");
}
