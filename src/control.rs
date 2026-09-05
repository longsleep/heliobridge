//! A local control API: HTTP over a Unix socket, off unless configured.
//!
//! Somewhere for commands to come from before Home Assistant does. The research proxy had a control FIFO
//! for exactly this purpose, and it earned its place — being able to change one setting and watch what
//! happens is how most of this protocol was worked out.
//!
//! ```text
//! HELIOBRIDGE_CONTROL_SOCKET=/run/heliobridge.sock heliobridge
//!
//! curl --unix-socket /run/heliobridge.sock http://local/devices
//! curl --unix-socket /run/heliobridge.sock http://local/devices/$SERIAL/settings
//! curl --unix-socket /run/heliobridge.sock -X PUT \
//!      http://local/devices/$SERIAL/settings/slot1_output_power \
//!      -H 'content-type: application/json' -d '{"value":100}'
//! ```
//!
//! ```text
//! GET  /healthz
//! GET  /devices                              { "devices": [ … ] }
//! GET  /devices/{device}                      what it is and what it is doing
//! GET  /devices/{device}/identity             every config register it reports
//! GET  /devices/{device}/telemetry            { "timestamp": …, "readings": [ … ] }
//! GET  /devices/{device}/telemetry/{key}      by field name or register number
//! GET  /devices/{device}/settings             { "settings": [ … ] }
//! GET  /devices/{device}/settings/{key}
//! PUT  /devices/{device}/settings/{key}       {"value": 100}
//! POST /devices/{device}/settings/{key}/read
//! POST /devices/{device}/config/read           ?registers=a,b,c or ?all — streamed as JSON Lines
//! ```
//!
//! # Shapes are consistent, so a client can be written against one
//!
//! A collection answers under a key naming it — `devices`, `settings`, `readings` — and a single resource
//! answers as a bare object. Errors are `application/problem+json` (RFC 9457) with `status`, `title` and
//! `detail`. Reads of cached state cost no device traffic; only `PUT` and `POST …/read` reach the device.
//!
//! Everything decoded is served, the serial and password fields included. This socket belongs to the
//! device's owner and its routes name the serial already, so withholding their own data would only put
//! fields that exist on the wire out of reach. Redaction applies to what gets committed, not to what runs.
//!
//! # Routes are scoped to a device
//!
//! One session per connection, each learning its own serial from CONNECT, and each relay connecting
//! upstream as *that* device. Nothing restricts this program to a single inverter, so nothing in the API
//! may assume one: a request names the device it is for, and [`Registry`] resolves it to the session that
//! can carry it out. A settings route with no device in it would work right up until someone added a
//! second inverter and then quietly address the wrong one.
//!
//! # A write returns what was actually stored
//!
//! `PUT` does not answer until the value has been read back off the device. That is the whole point: this
//! device silently clamps out-of-range writes, does not acknowledge single-register writes at all, and
//! changes `default_output_power` on its own when `power_plus` moves. A write reporting success on
//! transmission would be reporting something nobody asked about.
//!
//! ```json
//! { "name": "slot1_output_power", "register": 257, "requested": 100, "stored": 100, "confirmed": true }
//! ```
//!
//! `"confirmed": false` with a differing `stored` is the clamp, reported rather than hidden. It comes back
//! as `409 Conflict`: the request was carried out, the device simply did not do as asked.
//!
//! # Not a network service
//!
//! A Unix socket: reachability is filesystem permissions rather than anything this program implements, and
//! it is created mode 0600. Off by default, because a facility that changes settings on a mains-connected
//! battery inverter should exist only when asked for.
//!
//! # The allowlist is inherited, not re-implemented
//!
//! Every request becomes a [`Command`], which can only be built from the holding register map with a value
//! inside the register's domain. There is no path from this socket to a register the encoder would refuse.

use core::convert::Infallible;
use core::time::Duration;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{FromRequestParts, Query, RawPathParams, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::driver::catalogue::{Catalogue, ConfigField, Setting as SettingInfo};
use crate::driver::commands::Command;
use crate::model::{Raw, Register};

/// How many commands may queue per device before new ones are refused.
///
/// These arrive at human pace, and a backlog would mean applying settings long after they were asked for.
pub const QUEUE_DEPTH: usize = 8;

/// How long a request waits for the device before giving up.
///
/// A write is followed by a read-back, and the device answers a read in about 0.6 s — but it may be busy
/// with telemetry, and the first read of a session has been seen to take 4.6 s.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the control socket could not be set up.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ControlError {
    /// The socket could not be bound.
    #[snafu(display("could not bind the control socket at {}", path.display()))]
    Bind {
        /// The path attempted.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// What a request wants a session to do.
#[derive(Debug)]
pub enum Action {
    /// Apply a command and report what the device ended up holding.
    Apply(Command),
    /// Read a register and report its value.
    Refresh(Register),
    /// Transmit a config-space write and report only that it was sent.
    ///
    /// Deliberately unverified, unlike [`Self::Apply`]. A config write draws no acknowledgement, and the
    /// read that would confirm one has never been observed on the wire, so "sent" is the honest maximum.
    /// Two of these do not even hold a value to read back: registers 32 and 35 are actions.
    Send(Command),
}

/// One request, with somewhere to send the answer.
#[derive(Debug)]
pub struct Request {
    /// What to do.
    pub action: Action,
    /// Where the outcome goes. Dropped if the caller gave up.
    pub reply: oneshot::Sender<Outcome>,
}

/// What happened to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// Field name, where the register has one.
    pub name: Option<&'static str>,
    /// The register involved.
    pub register: u16,
    /// What was asked for, if anything was.
    pub requested: Option<u16>,
    /// What the device holds now, as read back.
    pub stored: Option<u16>,
    /// How the stored value reads.
    pub value: Option<String>,
    /// Whether the stored value is what was asked for.
    ///
    /// `false` with a differing `stored` is the device's silent clamp, surfaced.
    pub confirmed: bool,
    /// Set when the device never answered.
    pub error: Option<String>,
    /// Whether the driver refused to express the command at all, which is the caller's mistake.
    #[serde(skip_serializing_if = "core::ops::Not::not")]
    pub refused: bool,
}

impl Outcome {
    /// An outcome for a register the device did not answer about.
    pub fn timed_out(setting: &impl SettingInfo, requested: Option<Raw>) -> Self {
        Self {
            name: Some(setting.name()),
            register: setting.register().number(),
            requested: requested.map(Raw::get),
            stored: None,
            value: None,
            confirmed: false,
            error: Some("the device did not answer the read-back".to_owned()),
            refused: false,
        }
    }

    /// An outcome for a config command that was transmitted.
    ///
    /// `confirmed` is true because the request was carried out as far as the protocol allows: the frame went
    /// out. It does not claim the device acted on it — `error` carries that caveat rather than leaving the
    /// caller to infer it. A read is answered, but asynchronously, in an uplink frame that lands in the
    /// identity cache rather than here; a write is never answered at all.
    pub fn sent(command: &Command, field: Option<&impl ConfigField>) -> Self {
        let caveat = if matches!(command, Command::WriteConfig { .. }) {
            "sent; a config write draws no acknowledgement, so the device's action is unverified"
        } else {
            "sent; the answer arrives as a separate report, so read the register back to see it"
        };
        Self {
            confirmed: true,
            error: Some(caveat.to_owned()),
            ..Self::for_config(command, field)
        }
    }

    /// An outcome for a command that could not be transmitted.
    pub fn not_sent(command: &Command, field: Option<&impl ConfigField>, error: &str) -> Self {
        Self {
            confirmed: false,
            error: Some(error.to_owned()),
            ..Self::for_config(command, field)
        }
    }

    /// An outcome for a command the driver would not express.
    ///
    /// Distinct from [`Self::not_sent`] because the cause is: an unwritable register or a value out of
    /// range is the caller's mistake, and a caller can only tell if it is told.
    pub fn refused(command: &Command, field: Option<&impl ConfigField>, error: &str) -> Self {
        Self {
            refused: true,
            ..Self::not_sent(command, field, error)
        }
    }

    /// The register and value fields shared by both config outcomes.
    fn for_config(command: &Command, field: Option<&impl ConfigField>) -> Self {
        let value = match command {
            Command::WriteConfig { value, .. } => Some(value.clone()),
            _ => None,
        };
        Self {
            name: field.map(ConfigField::name),
            register: field.map_or(0, |field| field.register().number()),
            requested: None,
            stored: None,
            value,
            confirmed: false,
            error: None,
            refused: false,
        }
    }

    /// An outcome from a value read back off the device.
    pub fn read_back(setting: &impl SettingInfo, requested: Option<Raw>, stored: Raw) -> Self {
        Self {
            name: Some(setting.name()),
            register: setting.register().number(),
            requested: requested.map(Raw::get),
            stored: Some(stored.get()),
            value: Some(setting.decode(stored).to_string()),
            // Nothing requested means nothing to disagree with, so learning the value is success.
            confirmed: requested.is_none_or(|wanted| wanted == stored),
            error: None,
            refused: false,
        }
    }
}

/// One known setting, as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingView {
    /// Register number.
    pub register: u16,
    /// Field name.
    pub name: &'static str,
    /// Raw value as stored.
    pub raw: u16,
    /// Rendered value: a flag as 0/1, a slot boundary as `HH:MM`, a work mode as its label.
    pub value: String,
    /// Unit symbol, empty where there is none.
    pub unit: &'static str,
}

impl SettingView {
    /// Describe one setting's stored value.
    pub fn new(setting: &impl SettingInfo, raw: Raw) -> Self {
        Self {
            register: setting.register().number(),
            name: setting.name(),
            raw: raw.get(),
            value: setting.decode(raw).to_string(),
            unit: setting.unit().symbol(),
        }
    }
}

/// One config register as the datalogger reported it.
///
/// Every field is served, the serial and password included. This socket belongs to the device's owner —
/// its routes are keyed by the serial already — so filtering their own data out of their own API would only
/// make fields that exist on the wire unreachable. Redaction is a property of what gets committed, not of
/// what runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigView {
    /// Config register number. Its own address space: config 31 is the clock, holding 31 is nothing.
    pub register: u16,
    /// Documented field name, or `null` for a key the driver cannot name.
    pub name: Option<String>,
    /// What the field is for: identity, metadata, dynamic, endpoint, inert, or `null` when unknown.
    pub role: Option<String>,
    /// The value as sent. ASCII on the wire whatever the field means.
    pub value: String,
}

/// What the datalogger says about itself, from the report it sends on every connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityView {
    /// Entries the frame declared, which is also how many follow.
    pub declared: u16,
    /// Whether the body ran out before the declared count was reached.
    pub truncated: bool,
    /// The endpoint the device believes it should dial, assembled from three registers.
    pub endpoint: Option<String>,
    /// Every entry reported, in the order sent.
    pub entries: Vec<ConfigView>,
}

/// Which config registers a read is for.
#[derive(Debug, PartialEq, Eq)]
enum Selection {
    /// The whole space, `0..=CONFIG_REGISTER_LAST`.
    All,
    /// The keys named, each a field name or a register number.
    Named(Vec<String>),
}

/// Query parameters for the config read route.
#[derive(Debug, Deserialize)]
pub struct ReadParams {
    /// `?registers=` — comma-separated names or numbers.
    registers: Option<String>,
    /// `?all` — the whole space. Bare, or `all=true`; `all=false` reads as absent.
    all: Option<String>,
    /// `?batch=N` — how many registers per request frame. Defaults to 1, which is the only count the vendor
    /// server has ever been seen to send; the device honours more.
    batch: Option<usize>,
}

impl ReadParams {
    /// What the query asked for, or why it did not ask for anything usable.
    ///
    /// The two forms are exclusive rather than one taking precedence: a request naming both has two readings
    /// and guessing which was meant is how a caller ends up reading 146 registers by accident.
    fn selection(&self) -> Result<Selection, &'static str> {
        let all = self.all.as_deref().is_some_and(|value| {
            // Bare `?all` arrives as an empty value, which is the common spelling and means yes.
            !matches!(value.trim(), "false" | "0")
        });
        let named: Vec<String> = self.registers.as_deref().map_or_else(Vec::new, |list| {
            list.split(',')
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
                .collect()
        });
        match (all, named.is_empty()) {
            (true, true) => Ok(Selection::All),
            (false, false) => Ok(Selection::Named(named)),
            (true, false) => Err("name either ?registers= or ?all, not both"),
            (false, true) => Err("say what to read: ?registers= with names or numbers, or ?all"),
        }
    }
}

/// What a complete read of the configuration space found.
///
/// The space is bounded, so this is a terminating operation with a fixed cost rather than a probe: every
/// register from 0 to [`CONFIG_REGISTER_LAST`] is asked for exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadAllView {
    /// How many registers were asked for. Constant unless the session went away part-way through.
    pub requested: u16,
    /// How many the device has a value for afterwards, including those it volunteers unasked.
    pub answered: u16,
    /// Registers that answered nothing. Some are genuinely empty — reading the whole space on the reference
    /// device left three unpopulated — so this is an observation, not a list of failures.
    pub silent: Vec<u16>,
}

/// Reads the configuration space, yielding each register as the device answers for it.
///
/// **An async cursor, not a batch job.** `while let Some(entry) = reader.next().await` is the shape, which
/// is what `tokio`'s own receivers and most database cursors offer on stable Rust — `AsyncIterator` is not
/// stable, so this is the idiom rather than a `Stream` impl and a dependency to go with it.
///
/// Why it matters here: answers arrive tens of seconds behind the asking, so anything that collects
/// everything before returning makes the caller wait for the slowest register to say anything at all. A
/// cursor lets a summary be accumulated, a response be streamed, or a caller stop early, from one
/// implementation.
///
/// **Batching is an implementation detail and deliberately so.** Whether the device honours a request for
/// more than one register is unproven (see [`Command::read_config_many`]); if it ignores the count and
/// answers only the first, the cursor still yields whatever arrives and the caller cannot tell the
/// difference except in how long it takes. That is the point of putting the iterator boundary here.
/// Reads the configuration space, yielding each register as the device answers for it.
///
/// A [`Stream`], because that is what this is: a sequence produced over time, whose consumer should be able
/// to render, count or abandon it without waiting for the end. The asking runs as its own task, so a slow
/// consumer cannot stall the requests and a burst of answers cannot starve them either — the two were
/// interleaved in an earlier attempt and the interleaving lost 40 registers.
///
/// **Batching is an implementation detail and deliberately so.** Whether the device honours a request for
/// more than one register is a device question (see [`Command::read_config_many`]); if it ignored the count
/// and answered only the first, this stream would still yield whatever arrived, and the caller could not
/// tell except in how long it took.
struct ConfigReader;

impl ConfigReader {
    /// Gap between consecutive request frames.
    ///
    /// A device in production use answering requests it did not ask for. Four per second is the rate a
    /// hand-run pass used without the device showing any sign of noticing.
    const PACE: Duration = Duration::from_millis(250);

    /// How long answers must stop arriving, **after every request has gone out**, before the stream ends.
    ///
    /// Answers lag the asking badly: measured on a real device, requests finished in 39 s and answers were
    /// still landing 20 s later. The "after every request has gone out" part is load-bearing — applying this
    /// while requests were still queued ended a run at 106 of 146.
    const QUIET: Duration = Duration::from_secs(10);

    /// Cap on the whole operation, in case answers never stop or never come.
    const SETTLE_LIMIT: Duration = Duration::from_mins(5);

    /// How often to re-check when the identity channel is quiet.
    const POLL: Duration = Duration::from_millis(500);

    /// The registers named, and only those.
    ///
    /// Reading all of them is this with the whole space passed in, so one implementation serves both and a
    /// subset read cannot drift from the complete one in pacing, ending or output shape.
    fn of(handle: SessionHandle, wanted: Vec<Register>, batch: usize) -> impl Stream<Item = ConfigView> {
        let asking = Self::ask(handle.clone(), wanted.clone(), batch.max(1));
        Self::answers(handle, wanted, asking)
    }

    /// Send a request for each wanted register, paced, as a background task.
    ///
    /// Returns the task handle so the answer stream can tell when the asking is done — which is when its
    /// quiet timer becomes meaningful.
    fn ask(handle: SessionHandle, wanted: Vec<Register>, batch: usize) -> JoinHandle<()> {
        tokio::spawn(async move {
            for (index, chunk) in wanted.chunks(batch).enumerate() {
                if index > 0 {
                    tokio::time::sleep(Self::PACE).await;
                }
                // A refusal or timeout is not retried: the register simply goes unanswered, which the
                // summary reports as silent.
                drop(
                    handle
                        .carry_out(Action::Send(Command::ReadConfig {
                            registers: chunk.to_vec(),
                        }))
                        .await,
                );
            }
        })
    }

    /// Yield each wanted config entry the first time it appears, until the asking is done and answers stop.
    ///
    /// Filtered to what was asked for, which matters for a subset read: the accumulated identity already
    /// holds the 32 registers the device volunteers, and streaming those back to a caller who asked for one
    /// would answer a question nobody put.
    fn answers(handle: SessionHandle, wanted: Vec<Register>, asking: JoinHandle<()>) -> impl Stream<Item = ConfigView> {
        async_stream::stream! {
            let wanted: Vec<u16> = wanted.iter().copied().map(Register::number).collect();
            let mut identity = handle.identity.clone();
            let mut seen: Vec<u16> = Vec::new();
            let mut last_new = Instant::now();
            let deadline = Instant::now().checked_add(Self::SETTLE_LIMIT);

            loop {
                // Cloned out of the watch borrow before any await: holding a `Ref` across one would block
                // every writer.
                let snapshot = identity.borrow_and_update().clone();
                let mut fresh = Vec::new();
                if let Some(report) = snapshot {
                    for entry in report.entries {
                        if wanted.contains(&entry.register) && !seen.contains(&entry.register) {
                            seen.push(entry.register);
                            fresh.push(entry);
                        }
                    }
                }
                if !fresh.is_empty() {
                    last_new = Instant::now();
                }
                for entry in fresh {
                    yield entry;
                }

                if asking.is_finished() && last_new.elapsed() >= Self::QUIET {
                    break;
                }
                if deadline.is_none_or(|end| Instant::now() >= end) {
                    break;
                }
                drop(tokio::time::timeout(Self::POLL, identity.changed()).await);
            }
        }
    }
}

/// One telemetry register as last decoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadingView {
    /// Input register number.
    pub register: u16,
    /// Field name. `unknown_*` where the meaning is not established.
    pub name: &'static str,
    /// Raw register value, before scaling.
    pub raw: u16,
    /// Scaled and rendered value.
    pub value: String,
    /// Unit symbol, empty where there is none.
    pub unit: &'static str,
    /// How well the field's meaning is established: `observed`, `verified` or `inferred`.
    pub confidence: &'static str,
}

/// The most recent telemetry frame, decoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryView {
    /// The device's own timestamp for the frame, where it reported a plausible one.
    pub timestamp: Option<String>,
    /// Every input register the frame carried.
    pub readings: Vec<ReadingView>,
}

/// What a session is doing, for the device resource.
///
/// The parts only the session knows: who owns the clock, whether the relay is up, and how much has come
/// through. Everything else on the device resource is assembled from the identity report and the last
/// telemetry frame, which are published anyway.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct StatusView {
    /// Whether traffic is being relayed to the vendor cloud.
    pub relaying: bool,
    /// How much authority the cloud keeps, when relaying.
    pub relay_mode: Option<&'static str>,
    /// The device's own clock, as last reported.
    pub device_time: Option<String>,
    /// How far this server's clock is from the device's, in seconds.
    ///
    /// **Positive means this server is ahead**, matching `Skew::seconds`. Under a minute this is dominated
    /// by the lag between the device sampling and the frame arriving, not by clock error: across 232 428
    /// frames the device's stamp trailed receipt by a median of 7 s and ranged from -6 to +12. A magnitude
    /// beyond `Skew::SIGNIFICANT` is what indicates a real disagreement.
    pub clock_skew_seconds: Option<i64>,
    /// Telemetry frames decoded this session.
    pub telemetry_frames: u64,
    /// Settings read back this session.
    pub reads: u64,
}

/// Why an action could not be carried out.
///
/// About reaching the session, not about what the device did with it — a device that refused or clamped a
/// write answers with an [`Outcome`] saying so, which is a success here.
#[derive(Debug, Snafu, PartialEq, Eq)]
#[snafu(visibility(pub))]
pub enum RequestError {
    /// The session already has as many commands queued as it will take.
    #[snafu(display("the session's command queue is full"))]
    Busy,

    /// The session ended before answering.
    #[snafu(display("the device session ended before answering"))]
    Ended,

    /// The device never answered.
    #[snafu(display("no answer within {}s", REQUEST_TIMEOUT.as_secs()))]
    TimedOut,
}

/// How the API reaches one device's session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    /// Requests for that session to carry out.
    pub requests: mpsc::Sender<Request>,
    /// Its current settings, so a read needs no device traffic.
    pub settings: watch::Receiver<Vec<SettingView>>,
    /// What the datalogger last said about itself. Absent until the first report, about five seconds in.
    pub identity: watch::Receiver<Option<IdentityView>>,
    /// The most recent telemetry frame. Absent until the first one arrives, about a second in.
    pub telemetry: watch::Receiver<Option<TelemetryView>>,
    /// What the session is doing: relay, clock, counts.
    pub status: watch::Receiver<StatusView>,
}

impl SessionHandle {
    /// Hand an action to the session and wait for what the device did.
    ///
    /// The one path from any interface to the device, so a write from Home Assistant gets the same read-back
    /// confirmation as one from `curl` and neither can grow its own idea of what happened.
    ///
    /// # Errors
    ///
    /// [`RequestError`] if the session could not be reached or did not answer. A device that *refused* or
    /// clamped the write answers with an [`Outcome`] instead, since that is something it did rather than
    /// something that went wrong.
    pub async fn carry_out(&self, action: Action) -> Result<Outcome, RequestError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .try_send(Request { action, reply })
            .map_err(|_ignored| RequestError::Busy)?;

        match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_)) => Err(RequestError::Ended),
            Err(_) => Err(RequestError::TimedOut),
        }
    }
}

/// Which devices are connected, as published on every change.
///
/// A type rather than a bare `Vec<String>` so it can grow — when each device connected, which peer it
/// came from, how many sessions it has had — without changing the signature of everything that watches
/// it. Subscribers ask it questions instead of indexing a vector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connected {
    /// Device serials, sorted, so equality is meaningful and output is stable.
    devices: Vec<String>,
}

impl Connected {
    /// The connected serials, in a stable order.
    pub fn devices(&self) -> &[String] {
        &self.devices
    }

    /// Whether a device is connected.
    pub fn contains(&self, device: &str) -> bool {
        self.devices.iter().any(|known| known == device)
    }

    /// How many devices are connected.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether nothing is connected.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// Which devices are connected, and how to reach each.
///
/// Shared between the API and every session. A session registers itself once its serial is known, and
/// removes itself when it ends — by [`Registration`]'s `Drop`, so it happens on the error paths too.
#[derive(Debug, Clone)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
    /// Announces the connected set on every change, so a publisher can react rather than poll.
    changes: watch::Sender<Connected>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            changes: watch::Sender::new(Connected::default()),
        }
    }
}

/// The registry's contents: each device's handle, tagged with which registration owns it.
#[derive(Debug, Default)]
struct Inner {
    devices: HashMap<String, (Epoch, SessionHandle)>,
    next_epoch: u64,
}

/// Which registration owns a device's entry.
///
/// A registration removes the entry on drop only if it is still the one that put it there. Without that,
/// the ordering on a reconnect — new session registers, old session's guard drops a moment later — would
/// delete the live entry and leave a connected device unaddressable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Epoch(u64);

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Announce a session, replacing any earlier one for the same device.
    ///
    /// Replacing is right: the device reconnects aggressively, and a stale handle would accept requests
    /// nothing is listening to.
    ///
    /// Each registration carries an epoch, and dropping one removes the entry **only if it is still the
    /// current one**. Without that, the ordering on a reconnect — new session registers, then the old
    /// session's guard drops — would delete the live entry and leave a connected device unaddressable.
    pub fn register(&self, device_id: &str, handle: SessionHandle) -> Registration {
        // One counter for every device rather than one per device. An epoch is only ever compared against
        // the entry under the *same* key, so process-wide uniqueness is more than enough: device A holding
        // 0, 3, 7 while B holds 1, 2 answers "is this entry still mine" as well as contiguous numbering.
        let epoch = match self.inner.lock() {
            Ok(mut inner) => {
                let epoch = Epoch(inner.next_epoch);
                // Distinctness is the whole property, so the counter refuses to issue rather than repeat.
                // Reaching the end takes 2^64 reconnects.
                match inner.next_epoch.checked_add(1) {
                    Some(next) => {
                        inner.next_epoch = next;
                        inner.devices.insert(device_id.to_owned(), (epoch, handle));
                        Some(epoch)
                    }
                    None => None,
                }
            }
            // Nothing was inserted, so this registration owns no entry and must remove none.
            Err(_) => None,
        };
        if epoch.is_none() {
            tracing::error!(device = %device_id, "could not register the device; it will not be addressable");
        }
        self.announce();

        Registration {
            registry: self.clone(),
            device_id: device_id.to_owned(),
            epoch,
        }
    }

    /// Watch the connected set, for anything that must react to a device arriving or leaving.
    ///
    /// A `watch` rather than a broadcast: a subscriber wants the current set, not the history of how it
    /// got there, and one that falls behind should catch up to the truth rather than replay.
    pub fn watch(&self) -> watch::Receiver<Connected> {
        self.changes.subscribe()
    }

    /// Publish the connected set, skipping the wake-up when nothing changed.
    fn announce(&self) {
        let connected = Connected {
            devices: self.devices(),
        };
        self.changes.send_if_modified(|current| {
            if *current == connected {
                return false;
            }
            *current = connected;
            true
        });
    }

    /// Find a device's session.
    pub fn handle(&self, device_id: &str) -> Option<SessionHandle> {
        let inner = self.inner.lock().ok()?;
        inner.devices.get(device_id).map(|(_, handle)| handle.clone())
    }

    /// Every connected device, sorted so output is stable.
    pub fn devices(&self) -> Vec<String> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut names: Vec<String> = inner.devices.keys().cloned().collect();
        names.sort_unstable();
        names
    }
}

/// Removes a session from the registry when dropped, unless it has already been replaced.
#[derive(Debug)]
pub struct Registration {
    registry: Registry,
    device_id: String,
    /// `None` when registration did not take effect, in which case this owns no entry and removes none.
    epoch: Option<Epoch>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let (Some(epoch), Ok(mut inner)) = (self.epoch, self.registry.inner.lock())
            // Only if this registration is still the current one for the device.
            && inner
                .devices
                .get(&self.device_id)
                .is_some_and(|(current, _)| *current == epoch)
        {
            inner.devices.remove(&self.device_id);
        }
        // Outside the lock, and unconditional: `announce` compares before sending, so a drop that removed
        // nothing — the reconnect case, where a newer registration already owns the entry — is silent.
        self.registry.announce();
    }
}

/// A value to write.
#[derive(Debug, Deserialize)]
struct WriteBody {
    value: u16,
}

/// Start serving the control API.
///
/// # Errors
///
/// [`ControlError::Bind`] if the path cannot be bound.
pub fn listen<D: Catalogue>(path: &Path, registry: Registry, driver: Arc<D>) -> Result<(), ControlError> {
    // A leftover socket from a previous run would make binding fail. Removing it is safe: a socket file is
    // not data, and a live one would have gone when its owner exited.
    if path.exists() {
        drop(std::fs::remove_file(path));
    }

    let listener = UnixListener::bind(path).context(BindSnafu {
        path: path.to_path_buf(),
    })?;
    restrict(path);

    let router = Router::new()
        .route("/healthz", get(Api::health))
        .route(
            "/meter",
            get(Api::meter_state)
                .put(Api::meter_enable)
                .post(Api::meter_update)
                .delete(Api::meter_disable),
        )
        .route(
            "/devices/{device}/meter-reading",
            put(Api::put_meter_reading).delete(Api::delete_meter_reading),
        )
        .route("/devices", get(Api::devices::<D>))
        .route("/devices/{device}", get(Api::device))
        .route("/devices/{device}/identity", get(Api::identity))
        .route("/devices/{device}/telemetry", get(Api::telemetry))
        .route("/devices/{device}/telemetry/{key}", get(Api::reading))
        .route("/devices/{device}/settings", get(Api::settings))
        .route("/devices/{device}/settings/{key}", get(Api::setting).put(Api::write))
        .route("/devices/{device}/settings/{key}/read", post(Api::refresh))
        .route("/devices/{device}/actions", get(Api::actions::<D>))
        .route("/devices/{device}/actions/{key}", post(Api::act::<D>))
        .route("/devices/{device}/config/read", post(Api::read_config_set::<D>))
        .route(
            "/devices/{device}/config/{key}",
            get(Api::config::<D>).put(Api::write_config::<D>),
        )
        .route("/devices/{device}/config/{key}/read", post(Api::read_config::<D>))
        .with_state(ApiState { registry, driver });

    let socket = path.to_path_buf();
    tokio::spawn(async move {
        tracing::info!(path = %socket.display(), "control API listening");
        if let Err(error) = axum::serve(listener, router).await {
            tracing::warn!(%error, "control API stopped");
        }
    });

    Ok(())
}

/// Make the socket owner-only.
///
/// Best effort: a socket that cannot be restricted is still better than none, and the operator asked for it
/// explicitly.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(%error, "could not restrict the control socket's permissions");
        }
    }
}

/// An action the device can be asked to perform.
///
/// Config-space commands rather than settings: each is a write of `"1"` to a register that *does* something
/// and holds nothing. Kept as a closed enum rather than derived from the register map, because "this register
/// is writable" and "triggering this is a sensible thing to offer over an API" are different claims — the
/// retarget registers are writable and belong nowhere near a one-word `POST`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum DeviceAction {
    /// Restart the datalogger. Recoverable: it reboots and reconnects by itself.
    Restart,
    /// Reset the datalogger to factory defaults. **Destructive**: see [`Self::effect`].
    FactoryReset,
}

impl DeviceAction {
    /// Every action offered.
    const ALL: [Self; 2] = [Self::Restart, Self::FactoryReset];

    /// The name in the route.
    const fn name(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::FactoryReset => "factory-reset",
        }
    }

    /// What it does, in a sentence, for the listing.
    const fn effect(self) -> &'static str {
        match self {
            Self::Restart => {
                "reboots the datalogger; the session drops and returns within seconds, and telemetry pauses \
                 meanwhile. The inverter keeps running"
            }
            Self::FactoryReset => {
                "resets the datalogger to factory defaults. The serial, the clock and the server endpoint \
                 survive; the Wi-Fi credentials do not, so the device leaves the network and must be \
                 re-provisioned over Bluetooth, in person. The Bluetooth key returns to the published \
                 constant"
            }
        }
    }

    /// The config field behind it, as the catalogue names it.
    ///
    /// Not the same word as the route: the route reads `factory-reset` and the field is `factory_reset`,
    /// and neither should have to change for the other.
    const fn field(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::FactoryReset => "factory_reset",
        }
    }

    /// Find one by name.
    fn lookup(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

/// What every handler can reach: the sessions, and the driver that names what they hold.
///
/// The driver is here because resolving `"slot1_output_power"` to a register, or saying what a config
/// field is called, is catalogue knowledge — and a control API that had its own copy of it would be a
/// second table to disagree with the first.
struct ApiState<D> {
    /// The connected sessions.
    registry: Registry,
    /// The one driver this program serves.
    driver: Arc<D>,
}

// Derived `Clone` would demand `D: Clone`, which a driver has no reason to be.
impl<D> Clone for ApiState<D> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            driver: Arc::clone(&self.driver),
        }
    }
}

/// Why an extractor refused a request.
///
/// A status and a sentence rather than a built `Response`: a response is large enough that returning one in
/// an `Err` is worth a lint, and this keeps the rendering — the problem document — in exactly one place.
struct Rejection {
    /// What to answer with.
    code: StatusCode,
    /// What to tell the caller.
    detail: String,
}

impl Rejection {
    /// Refuse with a status and a message.
    fn new(code: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        problem(self.code, &self.detail)
    }
}

/// The session named in the route, resolved before the handler runs.
///
/// Every device-scoped route began with the same three lines — look the serial up, answer "not connected"
/// otherwise. That is a precondition rather than logic, and a precondition is what an extractor is for: a
/// handler taking a `Session` cannot run without one, so a new route cannot forget the check.
struct Session {
    /// The session that can carry a request out.
    handle: SessionHandle,
    /// The serial it is registered under, so a handler can name the device it just answered about.
    device: String,
}

impl<D: Send + Sync + 'static> FromRequestParts<ApiState<D>> for Session {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState<D>) -> Result<Self, Self::Rejection> {
        let registry = &state.registry;
        let device = path_param(parts, "device").await?;
        let handle = registry.handle(&device).ok_or_else(|| {
            Rejection::new(
                StatusCode::NOT_FOUND,
                format!("no connected device {device:?}; see /devices"),
            )
        })?;
        Ok(Self { handle, device })
    }
}

/// The `{key}` path segment, whatever it names.
///
/// Telemetry fields are not holding registers, so a reading is found by name rather than resolved to a
/// writable register. This carries the segment as sent.
struct Key(String);

impl<D: Send + Sync + 'static> FromRequestParts<ApiState<D>> for Key {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, _state: &ApiState<D>) -> Result<Self, Self::Rejection> {
        path_param(parts, "key").await.map(Self)
    }
}

/// The setting named in the route, resolved to a register before the handler runs.
struct Setting {
    /// The register the key resolved to.
    register: Register,
    /// The key as the caller wrote it, so a message can echo their own words back.
    key: String,
}

impl<D: Catalogue> FromRequestParts<ApiState<D>> for Setting {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState<D>) -> Result<Self, Self::Rejection> {
        let key = path_param(parts, "key").await?;
        let register = resolve(state.driver.as_ref(), &key)
            .ok_or_else(|| Rejection::new(StatusCode::NOT_FOUND, format!("unknown setting {key:?}")))?;
        Ok(Self { register, key })
    }
}

/// Read one named path parameter.
///
/// By name rather than by position, so a route that gains a segment cannot silently shift what an extractor
/// reads. A missing parameter is this program's mistake, not the caller's, hence the 500.
async fn path_param(parts: &mut Parts, name: &str) -> Result<String, Rejection> {
    let missing = || {
        Rejection::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("route is missing the {name:?} parameter"),
        )
    };
    // Through the extractor rather than by reading `parts.extensions`: the matched parameters live in a
    // private type there, so poking at extensions directly compiles and then finds nothing at runtime.
    let params = RawPathParams::from_request_parts(parts, &())
        .await
        .map_err(|_ignored| missing())?;
    params
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
        .ok_or_else(missing)
}

/// The HTTP surface.
///
/// A unit type carrying the handlers as associated functions rather than a module of loose `async fn`s: the
/// routes name `Api::identity` and `Api::write`, so what serves a route is findable from the route, and the
/// helpers they share sit with them instead of alongside every other free function in the file.
struct Api;

#[expect(
    clippy::unused_async,
    reason = "axum's Handler trait takes a function returning a future, so a handler that awaits nothing is \
              still async. Scoped to this impl rather than the crate, where an idle async fn is worth knowing \
              about."
)]
impl Api {
    /// Liveness, for a supervisor that wants a cheap check.
    async fn health() -> &'static str {
        "ok\n"
    }

    /// Which devices are connected right now.
    async fn devices<D: Send + Sync + 'static>(State(state): State<ApiState<D>>) -> Response {
        axum::Json(serde_json::json!({ "devices": state.registry.devices() })).into_response()
    }

    /// What the datalogger says about itself: firmware, model, network, clock, endpoint.
    ///
    /// Every field it reported, the serial and password included. From the report sent on every connect, so
    /// no device traffic.
    async fn identity(Session { handle, .. }: Session) -> Response {
        Self::cached(
            handle.identity.borrow().clone(),
            "no identity report yet; the device sends one on connect",
        )
    }

    /// The most recent telemetry frame, every register it carried.
    async fn telemetry(Session { handle, .. }: Session) -> Response {
        Self::cached(
            handle.telemetry.borrow().clone(),
            "no telemetry yet; the device publishes about a second after connecting",
        )
    }

    /// One telemetry reading, by field name or register number.
    async fn reading(Session { handle, .. }: Session, Key(key): Key) -> Response {
        let number = key.parse::<u16>().ok();
        let found = handle.telemetry.borrow().as_ref().and_then(|view| {
            view.readings
                .iter()
                .find(|reading| reading.name == key || Some(reading.register) == number)
                .cloned()
        });
        match found {
            Some(reading) => axum::Json(reading).into_response(),
            None => problem(StatusCode::NOT_FOUND, &format!("no telemetry reading {key:?}")),
        }
    }

    /// One device: what it is, what it is doing, and where it thinks it should connect.
    ///
    /// Assembled rather than stored — the identity report and the last telemetry frame are published for
    /// their own routes anyway, and duplicating a summary of them would give it its own way of being stale.
    async fn device(session: Session) -> Response {
        let identity = session.handle.identity.borrow().clone();
        let field = |name: &str| {
            identity
                .as_ref()
                .and_then(|report| report.entries.iter().find(|entry| entry.name.as_deref() == Some(name)))
                .map(|entry| entry.value.clone())
        };
        axum::Json(serde_json::json!({
            "device": session.device,
            "model": field("model_id"),
            "firmware": field("sw_version"),
            "hardware": field("hw_version"),
            "endpoint": identity.as_ref().and_then(|report| report.endpoint.clone()),
            "status": session.handle.status.borrow().clone(),
            "last_telemetry": session
                .handle
                .telemetry
                .borrow()
                .as_ref()
                .and_then(|view| view.timestamp.clone()),
        }))
        .into_response()
    }

    /// Every setting a device's session knows, from its cache. No device traffic.
    async fn settings(Session { handle, .. }: Session) -> Response {
        axum::Json(serde_json::json!({ "settings": handle.settings.borrow().clone() })).into_response()
    }

    /// One setting from the cache.
    async fn setting(Session { handle, .. }: Session, setting: Setting) -> Response {
        let found = handle
            .settings
            .borrow()
            .iter()
            .find(|view| view.register == setting.register.number())
            .cloned();

        match found {
            Some(view) => axum::Json(view).into_response(),
            // Known register, no value: either the startup read-back has not reached it, or it belongs to a
            // slot beyond `--slots`, which nothing reads. Both are "not available", which is what this says.
            None => problem(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "no value for {} yet; the startup read-back may still be in progress",
                    setting.key
                ),
            ),
        }
    }

    /// Write a setting and report what the device ended up holding.
    async fn write(
        Session { handle, .. }: Session,
        setting: Setting,
        body: Result<axum::Json<WriteBody>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        let Ok(axum::Json(body)) = body else {
            return problem(StatusCode::BAD_REQUEST, r#"expected a body like {"value":100}"#);
        };

        // `set` rather than `write`, so `default_output_power` goes out as the `321..322` range the vendor
        // uses rather than as a single-register write nobody has seen this device accept. Built here so a
        // refusal reads as a bad request rather than a device problem: the allowlist and the register's
        // domain are the encoder's decision, and both are the caller's mistake.
        dispatch(
            &handle,
            Action::Apply(Command::Set {
                register: setting.register,
                value: body.value,
            }),
        )
        .await
    }

    /// Force a read of one register.
    async fn refresh(Session { handle, .. }: Session, setting: Setting) -> Response {
        dispatch(&handle, Action::Refresh(setting.register)).await
    }

    /// One config register's value, as last reported.
    ///
    /// From the accumulated identity — no device traffic, like every other `GET` here.
    async fn config<D: Catalogue>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Key(key): Key,
    ) -> Response {
        let register = match Self::config_register(state.driver.as_ref(), &key) {
            Ok(register) => register,
            Err(rejection) => return rejection.into_response(),
        };
        let found = handle.identity.borrow().as_ref().and_then(|report| {
            report
                .entries
                .iter()
                .find(|entry| entry.register == register.number())
                .cloned()
        });
        match found {
            Some(entry) => axum::Json(entry).into_response(),
            None => problem(
                StatusCode::NOT_FOUND,
                &format!("the device has not reported config register {}", register.number()),
            ),
        }
    }

    /// Ask the device to report config registers, streamed as they answer.
    ///
    /// One route for a few registers or for the whole space, because they are one operation with different
    /// lists — a subset that answered differently from the whole would be a second implementation to keep
    /// honest.
    ///
    /// `?registers=` takes a comma-separated list of names or numbers, resolved the same way `{key}` is
    /// elsewhere. `?all` takes the whole space: **every** register, including the 32 the device volunteers
    /// on connect, since that report appears to be assembled once per session and asking again is the only
    /// way to know a value is current rather than however old the session is.
    ///
    /// Filtered to what was asked for: the accumulated identity already holds the volunteered registers, and
    /// none of those is an answer to a request for something else.
    async fn read_config_set<D: Catalogue>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Query(params): Query<ReadParams>,
    ) -> Response {
        let batch = params.batch.unwrap_or(1);
        let last = state.driver.config_last().number();
        match params.selection() {
            Ok(Selection::All) => {
                let wanted = (0..=last).map(Register).collect();
                Self::stream_config(handle, wanted, batch, "the whole config space")
            }
            Ok(Selection::Named(keys)) => {
                let mut wanted = Vec::new();
                for key in keys {
                    match Self::config_register(state.driver.as_ref(), &key) {
                        Ok(register) if register.number() <= last => wanted.push(register),
                        // Outside the space is worth naming rather than answering nothing: the bound is
                        // known, so a number past it cannot be a typo the device will resolve.
                        Ok(register) => {
                            return problem(
                                StatusCode::BAD_REQUEST,
                                &format!(
                                    "config register {} is outside the space, which ends at {last}",
                                    register.number()
                                ),
                            );
                        }
                        Err(rejection) => return rejection.into_response(),
                    }
                }
                Self::stream_config(handle, wanted, batch, "a set of config registers")
            }
            Err(detail) => problem(StatusCode::BAD_REQUEST, detail),
        }
    }

    /// Stream a set of config registers as JSON Lines, one object per register, then a summary.
    ///
    /// Shared by both read routes. The body is produced as answers arrive rather than collected first,
    /// because the device answers tens of seconds behind the asking and a caller should not have to wait for
    /// the slowest register before seeing the first.
    fn stream_config(handle: SessionHandle, wanted: Vec<Register>, batch: usize, what: &'static str) -> Response {
        let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(QUEUE_DEPTH);
        let asked: Vec<u16> = wanted.iter().copied().map(Register::number).collect();

        tokio::spawn(async move {
            let entries = ConfigReader::of(handle, wanted, batch);
            tokio::pin!(entries);
            let mut answered: Vec<u16> = Vec::new();
            while let Some(entry) = entries.next().await {
                answered.push(entry.register);
                let Ok(mut line) = serde_json::to_string(&entry) else {
                    continue;
                };
                line.push('\n');
                // A send error means the client hung up. Stop asking the device for answers nobody is
                // waiting for — the point of streaming is that the caller can leave.
                if tx.send(Ok(line)).await.is_err() {
                    tracing::debug!(sent = answered.len(), "config read abandoned by the client");
                    return;
                }
            }
            let summary = ReadAllView {
                requested: u16::try_from(asked.len()).unwrap_or(u16::MAX),
                answered: u16::try_from(answered.len()).unwrap_or(u16::MAX),
                silent: asked.into_iter().filter(|number| !answered.contains(number)).collect(),
            };
            tracing::info!(
                batch,
                requested = summary.requested,
                answered = summary.answered,
                silent = summary.silent.len(),
                "read {what}"
            );
            if let Ok(mut line) = serde_json::to_string(&summary) {
                line.push('\n');
                drop(tx.send(Ok(line)).await);
            }
        });

        (
            [(http::header::CONTENT_TYPE, "application/jsonl")],
            axum::body::Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response()
    }

    /// Ask the device to report one config register again.
    ///
    /// A `POST` rather than a query parameter on the `GET`, and for the same reason `…/settings/{key}/read`
    /// is one: it puts a frame on the wire. A `GET` is supposed to be safe, and this costs the device's
    /// attention, may time out, and cannot be repeated for free.
    ///
    /// It also cannot answer with the value. The reply arrives asynchronously as an identity report, which is
    /// folded into the accumulated picture — so this reports that the request went out, and the `GET` above is
    /// where the value appears.
    async fn read_config<D: Catalogue>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Key(key): Key,
    ) -> Response {
        let register = match Self::config_register(state.driver.as_ref(), &key) {
            Ok(register) => register,
            Err(rejection) => return rejection.into_response(),
        };
        dispatch(
            &handle,
            Action::Send(Command::ReadConfig {
                registers: vec![register],
            }),
        )
        .await
    }

    /// Resolve a config register by documented name or by number.
    ///
    /// Any register, not only the writable ones: reading has no side effect, which is the same reasoning that
    /// leaves the holding-register read unrestricted.
    fn config_register<D: Catalogue>(driver: &D, key: &str) -> Result<Register, Rejection> {
        if let Ok(number) = key.parse::<u16>() {
            return Ok(Register(number));
        }
        driver.config_named(key).map(|entry| entry.register()).ok_or_else(|| {
            Rejection::new(
                StatusCode::NOT_FOUND,
                format!("unknown config register {key:?}; see /devices/…/identity"),
            )
        })
    }

    /// What actions this device accepts.
    ///
    /// Listed rather than documented elsewhere, because the set depends on what has been observed rather than
    /// on what a register map contains: these are config-space commands, and each was captured from the
    /// vendor's own interface before being offered here.
    async fn actions<D: Catalogue>(State(state): State<ApiState<D>>, _session: Session) -> Response {
        let listed: Vec<_> = DeviceAction::ALL
            .iter()
            .filter_map(|action| {
                let field = state.driver.config_named(action.field())?;
                Some(serde_json::json!({
                    "action": action.name(),
                    "register": field.register().number(),
                    "value": field.action(),
                    "effect": action.effect(),
                    "confirmable": false,
                }))
            })
            .collect();
        axum::Json(serde_json::json!({ "actions": listed })).into_response()
    }

    /// Trigger one action.
    ///
    /// `POST`, not `PUT`: these are not idempotent in any useful sense — restarting twice restarts twice —
    /// and there is no resource whose state they set.
    async fn act<D: Catalogue>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Key(key): Key,
    ) -> Response {
        let Some(action) = DeviceAction::lookup(&key) else {
            let known: Vec<&str> = DeviceAction::ALL.iter().map(|action| action.name()).collect();
            return problem(
                StatusCode::NOT_FOUND,
                &format!("unknown action {key:?}; this device accepts {}", known.join(", ")),
            );
        };
        // Both halves come from the catalogue: which register the field is, and what value carries it out.
        // A driver whose device has no such field simply does not offer the action.
        let Some((register, value)) = state
            .driver
            .config_named(action.field())
            .and_then(|field| Some((field.register(), field.action()?.to_owned())))
        else {
            return problem(
                StatusCode::NOT_IMPLEMENTED,
                &format!("this driver has no {:?} action", action.name()),
            );
        };
        dispatch(&handle, Action::Send(Command::WriteConfig { register, value })).await
    }

    /// What the simulated meter is reporting.
    ///
    /// Not device-scoped: there is one simulated meter and the device fetches from it, so it is a property
    /// of this program rather than of a session. `served` is the answer to the experiment — a nonzero count
    /// means the device really does poll a meter it has been given.
    async fn meter_state() -> Response {
        let meter = &crate::server::meter::METER;
        axum::Json(serde_json::json!({
            "enabled": meter.enabled(),
            "watts": meter.watts(),
            "served": meter.served(),
        }))
        .into_response()
    }

    /// Start answering meter polls, reporting `watts`.
    ///
    /// Idempotent: a second `PUT` changes the figure without stopping and starting. Omitting `watts` keeps
    /// whatever was last reported, so a meter can be switched back on at the figure it had.
    async fn meter_enable(body: Option<axum::Json<serde_json::Value>>) -> Response {
        let meter = &crate::server::meter::METER;
        let watts = body
            .and_then(|axum::Json(body)| body.get("watts").and_then(serde_json::Value::as_i64))
            .unwrap_or_else(|| meter.watts());
        meter.enable(watts);
        tracing::info!(watts = meter.watts(), "the simulated meter is on");
        Self::meter_state().await
    }

    /// Stop answering meter polls.
    ///
    /// The figure is kept rather than cleared, so turning it back on resumes where it left off. What
    /// changes is that a non-TLS connection is dropped again, as it is when this has never been used.
    async fn meter_disable() -> Response {
        let meter = &crate::server::meter::METER;
        meter.disable();
        tracing::info!("the simulated meter is off");
        Self::meter_state().await
    }

    /// Report a different figure, without changing whether the meter answers.
    ///
    /// Separate from `PUT` so a load can be swept without re-stating that the meter is on, and so that
    /// sweeping one does not silently switch it on if it was off — which would be a change nobody asked
    /// for in the middle of a measurement.
    async fn meter_update(axum::Json(body): axum::Json<serde_json::Value>) -> Response {
        let meter = &crate::server::meter::METER;
        let Some(watts) = body.get("watts").and_then(serde_json::Value::as_i64) else {
            return problem(StatusCode::BAD_REQUEST, r#"expected a body like {"watts":250}"#);
        };
        if !meter.enabled() {
            return problem(
                StatusCode::CONFLICT,
                "the simulated meter is not answering; PUT to start it",
            );
        }
        meter.set_watts(watts);
        tracing::info!(watts, "the simulated meter reports a new figure");
        Self::meter_state().await
    }

    /// Write one config register.
    ///
    /// Deliberately narrow. Two whole classes are refused rather than exposed:
    ///
    /// - **Anything that retargets the device** (17, 18, 19). A wrong value there leaves a device that
    ///   listens on no port, reachable only by standing next to it with a Bluetooth client, and `0x18`
    ///   carries no acknowledgement so the write that strands it looks exactly like one that worked.
    /// - **Actions** (restart, factory reset). Those have their own endpoint, where the effect of each is
    ///   spelled out, and the factory reset is not something to reach by supplying a value.
    ///
    /// What is left is the clock and the accessory list, neither of which can lose the device.
    ///
    /// No read-back: the config space acknowledges nothing and answers no read for these, so the honest
    /// answer is that it was sent. Confirm with a read of the register afterwards.
    async fn write_config<D: Catalogue>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Key(key): Key,
        body: Result<axum::Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        let Ok(axum::Json(body)) = body else {
            return problem(StatusCode::BAD_REQUEST, r#"expected a body like {"value":"ADD:1-1-…"}"#);
        };
        let Some(value) = body.get("value").and_then(|value| value.as_str()) else {
            return problem(StatusCode::BAD_REQUEST, r#"expected a body like {"value":"ADD:1-1-…"}"#);
        };
        let writable_config = state.driver.writable_config();
        let Some(field) = writable_config.iter().find(|field| field.name() == key) else {
            let writable: Vec<&str> = writable_config
                .iter()
                .filter(|field| !field.is_retarget() && field.action().is_none())
                .map(ConfigField::name)
                .collect();
            return problem(
                StatusCode::NOT_FOUND,
                &format!(
                    "{key:?} is not a writable config register; this accepts {}",
                    writable.join(", ")
                ),
            );
        };
        if field.is_retarget() {
            return problem(
                StatusCode::FORBIDDEN,
                &format!(
                    "{key:?} moves the device to a different server, which has no remote recovery; \
                     this endpoint refuses it"
                ),
            );
        }
        if field.action().is_some() {
            return problem(
                StatusCode::FORBIDDEN,
                &format!("{key:?} is an action; POST it to the actions endpoint instead"),
            );
        }

        dispatch(
            &handle,
            Action::Send(Command::WriteConfig {
                register: field.register(),
                value: value.to_owned(),
            }),
        )
        .await
    }

    /// Supply a meter reading to the device, as a meter would.
    ///
    /// `PUT {"watts": <signed>}` — positive for import, negative for export. The datalogger writes four
    /// registers from 309 after polling a meter and this writes the same block. Its own resource rather
    /// than a writable register because these are not settings: they are a data channel with no read-back.
    ///
    /// **A reading expires after about two minutes, and nothing here refreshes it.** A caller supplying
    /// readings has to write again inside that window, from a figure it has actually measured; see
    /// [`crate::growatt::v7::meter`] for why that is deliberate rather than an omission.
    ///
    /// No read-back, because the device offers none for these registers: the honest report is that the
    /// write was sent. What the device made of it appears in telemetry as `meter_active_power`, and
    /// `meter_connected` says whether it currently holds a reading at all.
    async fn put_meter_reading(
        Session { handle, .. }: Session,
        body: Result<axum::Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        const EXPECTED: &str = r#"expected a body like {"watts":250}"#;

        let Ok(axum::Json(body)) = body else {
            return problem(StatusCode::BAD_REQUEST, EXPECTED);
        };
        let Some(watts) = body.get("watts").and_then(serde_json::Value::as_i64) else {
            return problem(StatusCode::BAD_REQUEST, EXPECTED);
        };
        let Ok(watts) = i32::try_from(watts) else {
            return problem(
                StatusCode::BAD_REQUEST,
                "watts is far outside anything this equipment sees",
            );
        };

        // Logged here or nowhere: these registers answer no read-back, so this line is the only record of
        // what the device was told.
        tracing::info!(watts, "supplying a meter reading");
        dispatch(&handle, Action::Send(Command::MeterReading { watts, valid: true })).await
    }

    /// Withdraw the supplied reading, telling the device its meter has gone.
    ///
    /// Writes the all-zero block the firmware itself writes for a meter that is not answering, so the
    /// device drops the reading at once rather than waiting out the expiry.
    ///
    /// A verb rather than a flag on the value, because `0 W` is a *valid* reading — the grid is balanced,
    /// and the device acts on it by holding its output. Conflating the two would make "my meter has gone"
    /// unsayable.
    async fn delete_meter_reading(Session { handle, .. }: Session) -> Response {
        tracing::info!("withdrawing the supplied meter reading");
        dispatch(&handle, Action::Send(Command::MeterReading { watts: 0, valid: false })).await
    }

    /// Serve a cached value, or explain that it has not arrived yet.
    ///
    /// The three cached endpoints differ only in which field they read and what is missing when it is empty,
    /// so the shape lives here once. `503` rather than `404`: the device sends all of these unprompted, so an
    /// empty cache means early, not absent.
    fn cached<T: Serialize>(value: Option<T>, missing: &str) -> Response {
        match value {
            Some(value) => axum::Json(value).into_response(),
            None => problem(StatusCode::SERVICE_UNAVAILABLE, missing),
        }
    }
}

/// Hand an action to a session and render its outcome as HTTP.
async fn dispatch(handle: &SessionHandle, action: Action) -> Response {
    match handle.carry_out(action).await {
        Ok(outcome) => {
            let code = if outcome.confirmed {
                StatusCode::OK
            } else if outcome.refused {
                // The driver would not express it: an unwritable register, a value outside its range.
                // The caller's mistake, and worth saying so rather than blaming the device.
                StatusCode::BAD_REQUEST
            } else {
                // The request was carried out; the device simply did not do what was asked. 409 says that
                // more precisely than either 200 or 500.
                StatusCode::CONFLICT
            };
            (code, axum::Json(outcome)).into_response()
        }
        // A timeout is the gateway's, not this server's: the request was accepted and the device upstream
        // did not answer.
        Err(error @ RequestError::TimedOut) => problem(StatusCode::GATEWAY_TIMEOUT, &error.to_string()),
        Err(error) => problem(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

/// Accept either a field name or a register number.
///
/// Names are what the specification uses and what a person will type; numbers are what the protocol uses
/// and what a script may already hold.
fn resolve<D: Catalogue>(driver: &D, key: &str) -> Option<Register> {
    if let Ok(number) = key.parse::<u16>() {
        return Some(Register(number));
    }
    driver.setting_named(key).map(|entry| entry.register())
}

/// A JSON error body, so a script does not have to parse prose.
fn problem(code: StatusCode, detail: &str) -> Response {
    // RFC 9457, minus the members that would be inventions here: no `type` URI, because there is no
    // documentation to point one at, and no `instance`, because a request to a local socket has no useful
    // identifier. `title` comes from the status itself rather than being written twice per call site.
    let body = serde_json::json!({
        "status": code.as_u16(),
        "title": code.canonical_reason().unwrap_or("Error"),
        "detail": detail,
    });
    let mut response = (code, axum::Json(body)).into_response();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/problem+json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Registry, SessionHandle, SettingView, StatusView, resolve};
    use crate::driver::catalogue::Catalogue as _;
    use crate::growatt::driver::Growatt;
    use crate::model::{Raw, Register};

    /// A handle plus the ends a session would keep, so nothing is dropped mid-test.
    fn handle() -> (
        SessionHandle,
        tokio::sync::mpsc::Receiver<super::Request>,
        tokio::sync::watch::Sender<Vec<SettingView>>,
    ) {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(4);
        let (settings_tx, settings_rx) = tokio::sync::watch::channel(Vec::new());
        let (_identity_tx, identity_rx) = tokio::sync::watch::channel(None);
        let (_telemetry_tx, telemetry_rx) = tokio::sync::watch::channel(None);
        let (_status_tx, status_rx) = tokio::sync::watch::channel(StatusView::default());
        (
            SessionHandle {
                requests: requests_tx,
                settings: settings_rx,
                identity: identity_rx,
                telemetry: telemetry_rx,
                status: status_rx,
            },
            requests_rx,
            settings_tx,
        )
    }

    #[test]
    fn settings_resolve_by_name_or_number() {
        // Against the real catalogue: the point of the test is that names resolve, not that a stub does.
        assert_eq!(resolve(&Growatt, "slot1_output_power"), Some(Register(257)));
        assert_eq!(resolve(&Growatt, "grid_power_allowed"), Some(Register(326)));
        assert_eq!(resolve(&Growatt, "326"), Some(Register(326)));
        // A number is taken at face value even if undocumented; the encoder decides whether it may be
        // written, and reading anything is harmless.
        assert_eq!(resolve(&Growatt, "321"), Some(Register(321)));
        assert_eq!(resolve(&Growatt, "nonsense"), None);
        // Slots beyond the first resolve too, so a nine-slot install is addressable.
        assert_eq!(resolve(&Growatt, "slot9_output_power"), Some(Register(297)));
    }

    #[test]
    fn a_registry_tracks_devices_and_forgets_them_when_sessions_end() {
        let registry = Registry::new();
        assert!(registry.devices().is_empty());

        let (first, _rx1, _s1) = handle();
        let registration = registry.register("0EXAMPLE00000001", first);
        assert_eq!(registry.devices(), vec!["0EXAMPLE00000001".to_owned()]);
        assert!(registry.handle("0EXAMPLE00000001").is_some());
        assert!(registry.handle("0EXAMPLE00000002").is_none());

        // A second device coexists rather than displacing the first.
        let (second, _rx2, _s2) = handle();
        let other = registry.register("0EXAMPLE00000002", second);
        assert_eq!(registry.devices().len(), 2);

        drop(registration);
        assert_eq!(registry.devices(), vec!["0EXAMPLE00000002".to_owned()]);
        drop(other);
        assert!(registry.devices().is_empty());
    }

    #[test]
    fn reconnecting_replaces_the_stale_handle() {
        // The device reconnects aggressively. A stale handle would accept requests nothing is listening to.
        let registry = Registry::new();
        let (first, mut rx1, _s1) = handle();
        let old = registry.register("0EXAMPLE00000001", first);
        let (second, _rx2, _s2) = handle();
        let _new = registry.register("0EXAMPLE00000001", second);

        assert_eq!(registry.devices().len(), 1);
        rx1.close();
        // Dropping the *old* registration must not remove the live entry.
        drop(old);
        assert_eq!(
            registry.devices(),
            vec!["0EXAMPLE00000001".to_owned()],
            "the replacement should survive the old registration going away"
        );
    }

    #[test]
    fn devices_reconnect_independently_of_each_other() {
        // Epochs come from one counter shared by every device, so a device's own epochs are not
        // contiguous. What must hold is that each entry is owned by the registration that inserted it,
        // whatever numbers the others consumed in between.
        let registry = Registry::new();
        let (a1, _ra1, _sa1) = handle();
        let (b1, _rb1, _sb1) = handle();
        let (a2, _ra2, _sa2) = handle();

        let a_old = registry.register("0EXAMPLE0000000A", a1);
        let _b = registry.register("0EXAMPLE0000000B", b1);
        let _a_new = registry.register("0EXAMPLE0000000A", a2);

        // A's replacement took epoch 2, with B's registration holding 1 in between.
        drop(a_old);
        assert_eq!(
            registry.devices(),
            vec!["0EXAMPLE0000000A".to_owned(), "0EXAMPLE0000000B".to_owned()],
            "dropping A's old registration must leave both devices addressable"
        );
    }

    #[test]
    fn the_connected_set_is_published_as_it_changes() {
        let registry = Registry::new();
        let mut watch = registry.watch();
        assert!(watch.borrow_and_update().is_empty());

        let (first, _rx, _s) = handle();
        let registration = registry.register("0EXAMPLE00000001", first);
        assert!(watch.has_changed().expect("the sender outlives this"));
        let connected = watch.borrow_and_update().clone();
        assert_eq!(connected.len(), 1);
        assert!(connected.contains("0EXAMPLE00000001"));

        drop(registration);
        assert!(watch.has_changed().expect("the sender outlives this"));
        assert!(watch.borrow_and_update().is_empty());
    }

    #[test]
    fn a_replaced_registration_going_away_publishes_nothing() {
        // A reconnect leaves the connected set unchanged, so a subscriber should not be woken to be told
        // the same thing twice.
        let registry = Registry::new();
        let (first, _rx1, _s1) = handle();
        let old = registry.register("0EXAMPLE00000001", first);

        let mut watch = registry.watch();
        watch.borrow_and_update();

        let (second, _rx2, _s2) = handle();
        let _new = registry.register("0EXAMPLE00000001", second);
        drop(old);

        assert!(
            !watch.has_changed().expect("the sender outlives this"),
            "the set never changed, so nothing should have been published"
        );
    }

    #[test]
    fn a_matching_read_back_confirms() {
        let outcome = Outcome::read_back(&Growatt.describe(Register(257)), Some(Raw(100)), Raw(100));
        assert!(outcome.confirmed);
        assert_eq!(outcome.requested, Some(100));
        assert_eq!(outcome.stored, Some(100));
        assert_eq!(outcome.name, Some("slot1_output_power"));
        assert_eq!(outcome.value.as_deref(), Some("100"));
        assert!(outcome.error.is_none());
    }

    #[test]
    fn a_clamped_write_is_reported_not_hidden() {
        // The case this whole read-back exists for: 1000 W stored as 800 because power_plus is clear.
        let outcome = Outcome::read_back(&Growatt.describe(Register(322)), Some(Raw(1000)), Raw(800));
        assert!(!outcome.confirmed);
        assert_eq!(outcome.requested, Some(1000));
        assert_eq!(outcome.stored, Some(800));
    }

    #[test]
    fn learning_a_value_nobody_requested_counts_as_success() {
        // Reading 322 after toggling power_plus: no expected value, only a stale one to replace.
        let outcome = Outcome::read_back(&Growatt.describe(Register(322)), None, Raw(800));
        assert!(outcome.confirmed);
        assert_eq!(outcome.requested, None);
        assert_eq!(outcome.stored, Some(800));
    }

    #[test]
    fn an_unanswered_read_back_is_an_error_not_a_confirmation() {
        let outcome = Outcome::timed_out(&Growatt.describe(Register(257)), Some(Raw(100)));
        assert!(!outcome.confirmed);
        assert!(outcome.stored.is_none());
        assert!(outcome.error.is_some());
    }

    #[test]
    fn a_setting_view_renders_by_domain() {
        let flag = Growatt
            .setting(Register(326))
            .map(|entry| SettingView::new(&entry, Raw(1)))
            .expect("documented");
        assert_eq!(flag.name, "grid_power_allowed");
        assert_eq!(flag.value, "1");

        let time = Growatt
            .setting(Register(254))
            .map(|entry| SettingView::new(&entry, Raw(0x173B)))
            .expect("documented");
        assert_eq!(time.value, "23:59");

        let mode = Growatt
            .setting(Register(256))
            .map(|entry| SettingView::new(&entry, Raw(2)))
            .expect("documented");
        assert_eq!(mode.value, "smart_self_use");

        assert_eq!(
            Growatt
                .setting(Register(257))
                .map(|entry| SettingView::new(&entry, Raw(100)))
                .map(|view| view.unit),
            Some("W")
        );

        // Undocumented registers have nothing to show.
        assert!(
            Growatt
                .setting(Register(321))
                .map(|entry| SettingView::new(&entry, Raw(0)))
                .is_none()
        );
    }

    #[test]
    fn outcomes_serialise_for_a_script_to_read() {
        let json = serde_json::to_string(&Outcome::read_back(
            &Growatt.describe(Register(257)),
            Some(Raw(100)),
            Raw(100),
        ))
        .expect("serialise");
        assert!(json.contains(r#""confirmed":true"#), "{json}");
        assert!(json.contains(r#""name":"slot1_output_power""#), "{json}");
        assert!(json.contains(r#""stored":100"#), "{json}");
    }
}
