//! What a frame turned out to say.
//!
//! # The driver tells, rather than the server asking
//!
//! A driver reads one of its own frames and calls the methods of a [`Sink`] the server implements. The
//! alternative — handing back a decoded document for the server to inspect — would mean agreeing on the
//! shape of every document a protocol has, which is exactly the knowledge that should stay on one side of
//! the seam. Here the driver keeps its own types: its telemetry record, its identity report, whatever it
//! decodes them from. What crosses is only what the server does something with.
//!
//! So these signatures are a deliberately small vocabulary. Adding to them means the server has found a new
//! thing to do, which is the right reason; a driver with a message no method describes reports it through
//! [`Sink::undecoded`] rather than the seam growing to fit it.
//!
//! # Borrowed, because the server keeps almost nothing
//!
//! Every record here borrows from the driver's own decoded value for the duration of one call. The server
//! publishes, logs and counts — all of which finish inside the call — and copies out the little it retains.

use core::fmt;

use crate::model::{Raw, Reading, Register, Timestamp};

use super::wire::Wire;

/// A telemetry frame, as far as the server is concerned.
#[derive(Debug)]
pub struct Telemetry<'a> {
    /// The device's own clock, where it reported a plausible one.
    pub at: Option<Timestamp>,
    /// Every reading the frame carried.
    pub readings: &'a [Reading],
    /// Whether this is a record the device held and replayed rather than current state.
    ///
    /// Replayed records have been seen over an hour stale, so a server must not feed one to live state.
    pub buffered: bool,
}

impl Telemetry<'_> {
    /// One reading's value by name, for the handful a log line names.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.readings
            .iter()
            .find(|reading| reading.name == name)
            .and_then(Reading::as_f64)
    }
}

/// One field of what a device says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field<'a> {
    /// Where the device keeps it.
    pub register: Register,
    /// What the driver calls it, if it knows.
    pub name: Option<&'a str>,
    /// What the field is for, in the driver's own words, for display only.
    pub role: Option<&'a str>,
    /// The value, as reported.
    pub value: &'a str,
}

/// A device's description of itself.
///
/// Reports come whole and in part: the device volunteers everything it knows on connect, and answers a
/// single-field question with a report of one. `fields` says which this is.
#[derive(Debug)]
pub struct Identity<'a> {
    /// How many fields the frame declared, which need not be how many arrived.
    pub declared: u16,
    /// Whether the frame ran out before the declared count was reached.
    pub truncated: bool,
    /// Where the device believes it should connect, if the driver can say.
    pub endpoint: Option<String>,
    /// One line naming what matters, composed by the driver.
    ///
    /// A report carries fields no log line should repeat — serials, a password, hardware addresses — so
    /// what is safe to say is the driver's judgement rather than a server's guess.
    pub summary: String,
    /// The fields that parsed, in the order sent.
    pub fields: Vec<Field<'a>>,
}

impl Identity<'_> {
    /// One field's value by name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.name == Some(name))
            .map(|field| field.value)
    }
}

/// The device's own account of a range of its settings, arriving unprompted.
#[derive(Debug)]
pub struct Snapshot<'a> {
    /// First register the frame covers.
    pub first: Register,
    /// Last register the frame covers.
    pub last: Register,
    /// The values it carried, in register order.
    pub values: &'a [(Register, Raw)],
}

/// The device's answer to a write.
///
/// Informative rather than authoritative: an acknowledgement can report acceptance for a value the device
/// clamped, so a read-back still decides. A refusal is worth saying out loud, being the one case where the
/// device volunteers that it did not do as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAck {
    /// First register acknowledged.
    pub first: Register,
    /// Last register acknowledged, equal to `first` for a single write.
    pub last: Register,
    /// Whether the device reported accepting it.
    pub accepted: bool,
    /// The value the device reports holding, where the acknowledgement carries one.
    pub value: Option<Raw>,
    /// The driver's own word for the outcome, for a log line. Empty when it has nothing to add.
    pub status: String,
}

/// What a server wants to be told about the frames a device sends.
///
/// Implemented by the server. Every method has a default that does nothing, so a caller interested in
/// telemetry alone says so by implementing telemetry alone.
pub trait Sink {
    /// A telemetry frame decoded.
    fn telemetry(&mut self, telemetry: &Telemetry<'_>) {
        let _ = telemetry;
    }

    /// The device describing itself, in whole or in part.
    fn identity(&mut self, identity: &Identity<'_>) {
        let _ = identity;
    }

    /// The device answering a read of one setting.
    fn read_answer(&mut self, register: Register, value: Raw) {
        let _ = (register, value);
    }

    /// The device answering a write.
    fn write_ack(&mut self, ack: &WriteAck) {
        let _ = ack;
    }

    /// The device volunteering a range of its settings.
    fn snapshot(&mut self, snapshot: &Snapshot<'_>) {
        let _ = snapshot;
    }

    /// A frame the driver recognises but does not decode, named in the driver's own words.
    fn undecoded(&mut self, kind: &str, len: usize) {
        let _ = (kind, len);
    }

    /// A frame whose kind the driver does not recognise at all, described as best it can.
    ///
    /// Distinct from [`Self::undecoded`]: that is a known message this build has not got round to, this is
    /// something nobody has seen before, and the two deserve different attention.
    fn unknown(&mut self, kind: &str, len: usize) {
        let _ = (kind, len);
    }

    /// A frame that should have decoded and did not, which is a fault worth counting.
    fn unreadable(&mut self, kind: &str, error: &dyn fmt::Display) {
        let _ = (kind, error);
    }
}

/// Reading a driver's own frames aloud.
pub trait Report: Wire {
    /// Tell `to` what this frame says.
    ///
    /// Exactly one method of the sink is called for a frame the driver understands, and
    /// [`Sink::undecoded`] or [`Sink::unreadable`] for one it does not. Nothing is called twice, so a
    /// server can count what it is told.
    fn report(&self, frame: &Self::Frame<'_>, to: &mut dyn Sink);
}
