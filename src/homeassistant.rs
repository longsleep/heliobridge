//! Publishing to Home Assistant over an MQTT broker.
//!
//! This is the half of the program that faces the house rather than the device: it connects to a broker
//! Home Assistant already listens to, announces the device and its entities through MQTT discovery,
//! publishes readings as they arrive, and accepts commands back.
//!
//! # It is a client of the control plane, not a second copy of it
//!
//! Everything here goes through the same [`crate::control`] channels the socket API uses — the same
//! `watch` receivers for telemetry and settings, the same request channel for writes, so a write from
//! Home Assistant gets the same allowlist and the same read-back confirmation as one from `curl`. There
//! is no second path to the device, which is what keeps the two interfaces from disagreeing about what
//! the device holds.
//!
//! # Layout
//!
//! - [`broker`] — the MQTT client itself: connect, retry, publish, subscribe. It knows nothing about
//!   Home Assistant and would serve any broker; it lives here because nothing else needs it yet.
//! - [`publisher`] — what to say and when: topic names, availability, and the task that reacts to
//!   devices arriving and leaving.

pub mod broker;
pub mod publisher;
