//! The device-facing server: the thing the Nexa 2000 connects to instead of the vendor cloud.
//!
//! # The bridge *is* the MQTT server
//!
//! Existing bridges are MQTT clients, which obliges an operator to run a TLS-enabled broker for the
//! device to connect to and then subscribe to it. Eliminating that broker means being the server the
//! device talks to — and this is affordable because the device is a single client using nine packet
//! types with fully specified behaviour. No retained messages, no wildcards, no multi-client routing,
//! no QoS 2, no persistent sessions.
//!
//! Everything here is device-facing. The outbound side — publishing to the operator's own broker for
//! Home Assistant, and the optional relay to the vendor cloud — belongs to a future `bridge` module and
//! uses a client library rather than this code.
//!
//! - [`session`] — the per-connection state machine.
//! - [`listener`] — accept loop, one session per connection.
//! - [`access`] — who may connect: a source-address allowlist and a device-serial allowlist.
//! - [`tls`] — server TLS, including first-run certificate generation.
//! - [`clock`] — wall-clock time for the server time push, and the skew check that goes with it.
//!
//! The MQTT packet codec lives in [`crate::mqtt`], not here: the vendor cloud client speaks the same
//! protocol, and a codec owned by one of its two users would be in the wrong place.

pub mod access;
pub mod clock;
pub mod listener;
pub mod session;
pub mod tls;

pub use access::{AccessError, Devices, Peers};
pub use clock::Clock;
pub use listener::{ListenerError, SessionOptions, serve};
pub use session::{Session, SessionError, SessionStats};
pub use tls::{CertificateOrigin, TlsError, client_identity, server_config};
