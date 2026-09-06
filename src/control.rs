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
//! GET  /devices/{device}/accessories           everything enrolled, on either transport
//! POST /devices/{device}/accessories/network/discovered/search  {"model":"…"} — candidates as JSON Lines
//! POST /devices/{device}/accessories/network/discovered  confirm one: {"serial":"…","access":0}
//! GET  /devices/{device}/accessories/network/discovered/{entry}   one record
//! DELETE /devices/{device}/accessories/network/discovered/{entry}  remove it
//! POST /devices/{device}/accessories/lora/pair        open the radio's pairing window
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
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::driver::accessories::{Enrolled, Enrols};
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
    ///
    /// **Accumulated**, not one report: the device volunteers 32 registers on connect and answers reads one
    /// at a time, and this is everything it has said so far. So an entry being present says the device
    /// reported that register at some point, not that it just did — which is what [`Self::reported`] is for.
    pub entries: Vec<ConfigView>,
    /// The registers the *latest* report carried, which is the only way to tell a value that just arrived
    /// from one sitting in the accumulated view.
    ///
    /// It matters for a register the device volunteers rather than answers for: config 123 reports what an
    /// accessory search found, is **not** cleared afterwards, and so keeps its last value indefinitely.
    /// Reading the accumulated entry cannot distinguish "found now" from "found an hour ago", and treating
    /// a stale one as a discovery would invent an accessory that nothing is offering. Empty when the view is
    /// republished without a new report behind it.
    pub reported: Vec<u16>,
}

/// What an accessory the device polls last reported about itself.
///
/// From the accessory's own telemetry frame, which is the **only** place two of these appear. `access` is
/// what the accessory was enrolled with — it is absent from the accessory list, so this is the one way to
/// read it back — and the manufacturer and model are the device's own vocabulary for what it is talking to.
///
/// Note that `access` and `communicating` answer different questions and both matter: an accessory enrolled
/// with `access` 1 communicates perfectly and its reading is never used.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessoryView {
    /// What the accessory is, in the device's own vocabulary.
    pub manufacturer: String,
    /// Its model code, as the device spells it — a vendor code, unrelated to the accessory type a search
    /// names.
    pub model: String,
    /// Its serial, as the device identifies it: the decimal MAC.
    pub serial: String,
    /// What it was enrolled with. `0` means the device uses the reading; see the enrolment routes.
    pub access: u16,
    /// Whether the device is managing to read it.
    pub communicating: bool,
    /// Whether it is reporting a fault.
    pub faulted: bool,
    /// Total active power, watts, where it measures one.
    pub active_power: Option<f64>,
}

/// How a device reaches an accessory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Searched for by mDNS on the local network, then polled by address.
    Network,
    /// Adopted over the device's own radio.
    Lora,
}

/// One accessory, as the enrolment routes report it.
///
/// Assembled from three places, because no one of them can answer the question a caller has. The list
/// register says the state and the address; the accessory's own report says what it is and what it was
/// enrolled with; telemetry says whether the reading is actually in use. A field is absent rather than
/// guessed when its source has not spoken.
#[derive(Debug, Clone, Serialize)]
pub struct AccessoryEntryView {
    /// Which way the device reaches it.
    pub transport: Transport,
    /// How it got into the list, and so what can be done with it: `discovered` for the one the enrolment
    /// routes manage, `dialled` for an accessory the server gave an address for. Several of the latter can
    /// coexist and none of them is touched by those routes.
    pub kind: &'static str,
    /// The device's own number for this record. **This is what addresses it**: a delete names it, and the
    /// routes under `…/discovered/{entry}` take it. It is assigned by the device and cannot be predicted,
    /// so it is read from here and used, never guessed.
    pub entry: u16,
    /// The mode, which says how the device reaches the accessory.
    pub mode: u16,
    /// The entry's second field as the device spells it: a serial for a discovered accessory, a name for
    /// one reached by address.
    pub name: Option<String>,
    /// The device's own state, as a word.
    pub state: &'static str,
    /// The same state as the device's number, for a caller that wants it unrendered.
    pub state_code: u16,
    /// The accessory's serial, in the protocol's own form. A string because it is wider than a JSON number
    /// can carry through every encoder.
    pub serial: Option<String>,
    /// The same value as a MAC.
    pub mac: Option<String>,
    /// Where the device found it.
    pub address: Option<String>,
    /// Whether the device is using the reading. `None` for an entry it is not polling, or before telemetry
    /// has said. **Not** derivable from the list: see the enrolment routes.
    pub in_use: Option<bool>,
    /// What the accessory says it is, once it has reported.
    pub manufacturer: Option<String>,
    /// Its model code, in the vendor's vocabulary rather than the search's.
    pub model: Option<String>,
    /// What it was enrolled with, echoed back by the accessory itself.
    pub access: Option<u16>,
    /// Whether the device is managing to read it.
    pub communicating: Option<bool>,
}

/// The accessory searches this bridge has open.
///
/// The one piece of state this module keeps, for the one thing the protocol cannot answer: the device's
/// search is a single register, so a second caller must be able to *join* a window rather than restart it
/// under the first, and nothing on the wire says whether one is open.
///
/// It is deliberately not authoritative, and nothing depends on it being complete. A window opened by the
/// vendor's application, or before this program started, is not in here — the consequence is a redundant
/// search command, which is harmless. A window expires on its own after the device's own sixty seconds, so
/// nothing has to clean it up. Nothing *else* is remembered: an earlier draft kept the search that enrolled
/// each accessory, because the delete appeared to need it, and it turned out the device reads no such thing.
#[derive(Debug, Clone, Default)]
struct Searches(Arc<Mutex<HashMap<String, Searching>>>);

/// One device's search.
#[derive(Debug, Clone)]
struct Searching {
    /// What it asked for, which the delete route also needs: the device's delete names the search.
    target: (String, u16),
    /// When the device's own window closes.
    until: Instant,
}

/// What opening a search did.
struct Opened {
    /// Whether a search command has to be sent, as against joining one already running.
    started: bool,
    /// When to stop streaming.
    until: Instant,
}

impl Searches {
    /// Start a search, or join the one already open for the same target.
    fn open(&self, device: &str, target: &(String, u16), window: Duration) -> Opened {
        let now = Instant::now();
        let until = now.checked_add(window).unwrap_or(now);
        let Ok(mut searches) = self.0.lock() else {
            // A poisoned lock must not stop an owner enrolling a meter: start a search and stream it.
            return Opened { started: true, until };
        };
        if let Some(open) = searches.get_mut(device) {
            if open.until > now && open.target == *target {
                return Opened {
                    started: false,
                    until: open.until,
                };
            }
            open.target = target.clone();
            open.until = until;
            return Opened { started: true, until };
        }
        searches.insert(
            device.to_owned(),
            Searching {
                target: target.clone(),
                until,
            },
        );
        Opened { started: true, until }
    }

    /// Mark the window closed, for a search that could not be sent.
    fn close(&self, device: &str) {
        if let Ok(mut searches) = self.0.lock()
            && let Some(open) = searches.get_mut(device)
        {
            open.until = Instant::now();
        }
    }
}

/// Which config registers a read is for.
#[derive(Debug, PartialEq, Eq)]
enum Selection {
    /// The whole space, from zero to the last register the driver's catalogue names.
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
/// register from zero to the catalogue's last is asked for exactly once.
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
    /// What an enrolled accessory last reported about itself. Absent until one does, which is once a
    /// minute and only while one is paired.
    pub accessory: watch::Receiver<Option<AccessoryView>>,
    /// Fired when this session is displaced by a newer one for the same device.
    ///
    /// A session cannot tell on its own that the device has reconnected: its socket is half-open, reads
    /// simply stop, and it goes on holding a cloud relay that the vendor keeps delivering commands to —
    /// which are then written to a dead socket and lost. The registry knows, because it is the thing that
    /// replaced it, so it says so.
    pub stop: Arc<Notify>,
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
///
/// Each serial is paired with the session serving it, and **that pair is what makes a reconnect visible**.
/// A device reconnecting does not change which devices are connected: the registry replaces the session
/// behind the same serial, so a set of serials alone compares equal and the change is never announced.
/// Anything holding one session's channels would then go on holding the replaced one's — which is what
/// left Home Assistant reading `offline` for as long as a half-open socket took to time out.
///
/// The session is not published, only compared. A subscriber that has just been woken should ask the
/// registry which session serves a device rather than read it off a snapshot that may already be one
/// reconnect out of date.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connected {
    /// Each device and the session serving it, sorted by serial, so equality is meaningful and output is
    /// stable.
    devices: Vec<(String, SessionId)>,
}

impl Connected {
    /// The connected serials, in a stable order.
    pub fn devices(&self) -> impl ExactSizeIterator<Item = &str> {
        self.devices.iter().map(|(device, _)| device.as_str())
    }

    /// Whether a device is connected.
    pub fn contains(&self, device: &str) -> bool {
        self.devices.iter().any(|(known, _)| known == device)
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
    devices: HashMap<String, (SessionId, SessionHandle)>,
    next_session: u64,
}

/// Which session a device's entry belongs to.
///
/// Two jobs, both about telling one session for a serial from the next. A registration removes the entry
/// on drop only if it is still the one that put it there: without that, the ordering on a reconnect — new
/// session registers, old session's guard drops a moment later — would delete the live entry and leave a
/// connected device unaddressable. And anything that holds a session's channels can compare what it holds
/// against what is serving the device now, rather than wait for the replaced session to notice it is dead.
///
/// Distinct rather than meaningful: the number says nothing except which registration, and is only ever
/// compared for equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(u64);

impl SessionId {
    /// The only session there is, for a test that stands in for a registration rather than making one.
    #[cfg(test)]
    pub(crate) const fn sole() -> Self {
        Self(0)
    }
}

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
    /// Each registration carries a session id, and dropping one removes the entry **only if it is still the
    /// current one**. Without that, the ordering on a reconnect — new session registers, then the old
    /// session's guard drops — would delete the live entry and leave a connected device unaddressable.
    pub fn register(&self, device_id: &str, handle: SessionHandle) -> Registration {
        // One counter for every device rather than one per device. A session id is only ever compared
        // against the entry under the *same* key, so process-wide uniqueness is more than enough: device A
        // holding 0, 3, 7 while B holds 1, 2 answers "is this entry still mine" as well as contiguous
        // numbering would.
        // Taken under the lock and signalled outside it: a session that is ending should not be waited on
        // while the registry every other session needs is held.
        let mut displaced: Option<SessionHandle> = None;
        let session = match self.inner.lock() {
            Ok(mut inner) => {
                let session = SessionId(inner.next_session);
                // Distinctness is the whole property, so the counter refuses to issue rather than repeat.
                // Reaching the end takes 2^64 reconnects.
                match inner.next_session.checked_add(1) {
                    Some(next) => {
                        inner.next_session = next;
                        displaced = inner
                            .devices
                            .insert(device_id.to_owned(), (session, handle))
                            .map(|(_, handle)| handle);
                        Some(session)
                    }
                    None => None,
                }
            }
            // Nothing was inserted, so this registration owns no entry and must remove none.
            Err(_) => None,
        };
        if session.is_none() {
            tracing::error!(device = %device_id, "could not register the device; it will not be addressable");
        }
        // The device has reconnected, so whatever was serving it before is serving a socket that is gone.
        // Nothing else tells that session: its reads stop rather than fail, and it would go on taking
        // cloud commands and writing them nowhere until a write finally errored.
        if let Some(displaced) = displaced {
            tracing::info!(device = %device_id, "a newer session replaced this device's; stopping the old one");
            displaced.stop.notify_one();
        }
        self.announce();

        Registration {
            registry: self.clone(),
            device_id: device_id.to_owned(),
            session,
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
            devices: self.registered(),
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
        self.session(device_id).map(|(_, handle)| handle)
    }

    /// Find a device's session, and which session it is.
    ///
    /// Both under one lock, because a caller that took the identity from a [`Connected`] snapshot and the
    /// handle from here could pair a serial with a session that no longer serves it — and then believe it
    /// is up to date while holding channels nobody writes to.
    pub fn session(&self, device_id: &str) -> Option<(SessionId, SessionHandle)> {
        let inner = self.inner.lock().ok()?;
        inner
            .devices
            .get(device_id)
            .map(|(session, handle)| (*session, handle.clone()))
    }

    /// Every connected device, sorted so output is stable.
    pub fn devices(&self) -> Vec<String> {
        self.registered().into_iter().map(|(device, _)| device).collect()
    }

    /// Every connected device with the session serving it, sorted so output is stable.
    fn registered(&self) -> Vec<(String, SessionId)> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut registered: Vec<(String, SessionId)> = inner
            .devices
            .iter()
            .map(|(device, (session, _))| (device.clone(), *session))
            .collect();
        registered.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        registered
    }
}

/// Removes a session from the registry when dropped, unless it has already been replaced.
#[derive(Debug)]
pub struct Registration {
    registry: Registry,
    device_id: String,
    /// `None` when registration did not take effect, in which case this owns no entry and removes none.
    session: Option<SessionId>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let (Some(session), Ok(mut inner)) = (self.session, self.registry.inner.lock())
            // Only if this registration is still the current one for the device.
            && inner
                .devices
                .get(&self.device_id)
                .is_some_and(|(current, _)| *current == session)
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
pub fn listen<D: Catalogue + Enrols>(path: &Path, registry: Registry, driver: Arc<D>) -> Result<(), ControlError> {
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
            "/devices/{device}/meter-reading",
            put(Api::put_meter_reading).delete(Api::delete_meter_reading),
        )
        .route(
            "/devices/{device}/accessories/lora/pair",
            post(Api::pair_lora_accessory),
        )
        .route("/devices/{device}/accessories", get(Api::accessories::<D>))
        .route(
            "/devices/{device}/accessories/network/discovered/search",
            post(Api::search_discovered::<D>),
        )
        .route(
            "/devices/{device}/accessories/network/discovered",
            post(Api::pair_discovered::<D>),
        )
        .route(
            "/devices/{device}/accessories/network/discovered/{entry}",
            get(Api::discovered_one::<D>).delete(Api::forget_discovered::<D>),
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
        .with_state(ApiState {
            registry,
            driver,
            searches: Searches::default(),
        });

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
    /// Accessory searches in progress. See [`Searches`] for why this is the one thing kept here.
    searches: Searches,
}

// Derived `Clone` would demand `D: Clone`, which a driver has no reason to be.
impl<D> Clone for ApiState<D> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            driver: Arc::clone(&self.driver),
            searches: self.searches.clone(),
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

/// The `{entry}` path segment: the device's own number for one accessory record.
///
/// A number rather than a name because that is what the device's own list and its delete command use, and
/// what `GET …/accessories` reports for every entry.
struct Entry(u16);

impl<D: Send + Sync + 'static> FromRequestParts<ApiState<D>> for Entry {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, _state: &ApiState<D>) -> Result<Self, Self::Rejection> {
        let raw = path_param(parts, "entry").await?;
        raw.parse().map(Self).map_err(|_ignored| {
            Rejection::new(
                StatusCode::BAD_REQUEST,
                format!("{raw:?} is not an accessory entry number; see GET …/accessories"),
            )
        })
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
    /// How long to wait for a config register to be reported after asking for it.
    ///
    /// A config write draws no acknowledgement and the answer arrives as a separate report, so everything
    /// that reports what the device did waits at least this long before believing a read.
    const READ_BACK: Duration = Duration::from_secs(8);

    /// How long to leave between read-backs while waiting for a config write to show up.
    const READ_BACK_PACE: Duration = Duration::from_secs(2);

    /// How long to wait for the device to start using a newly paired accessory's reading.
    ///
    /// Polling has been observed beginning within ten seconds, and the reading follows on the next
    /// telemetry cycle. Long enough to answer the question the caller asked; short enough that a failure is
    /// reported rather than hung on.
    const PAIR_SETTLE: Duration = Duration::from_secs(30);

    /// How often a search stream re-checks, when no report has woken it.
    const SEARCH_POLL: Duration = Duration::from_millis(500);

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
                        // Refused here rather than passed on. The device does not refuse a high key: a
                        // key of 500 or more reaches a second store nothing has ever read, so a typo
                        // would write somewhere whose effect cannot be looked at afterwards.
                        Ok(register) => {
                            return problem(
                                StatusCode::BAD_REQUEST,
                                &format!(
                                    "config register {} is past {last}, the last one this build knows; \
                                     higher keys reach a store nothing here can read",
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
    /// readings has to write again inside that window, from a figure it has actually measured. Refreshing
    /// it here would mean this program asserting a measurement nobody took, which is the one thing a
    /// supplied reading must never be.
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

    /// Open a pairing window on the device's **LoRa radio**.
    ///
    /// **A segment per transport**, so this is `accessories/lora/pair` and never `accessories/pair`. The
    /// radio has one way in, so it needs no mechanism below it; the local network has two and they are
    /// grouped under `accessories/network/`. Sharing one level would put this one-shot action beside a
    /// staged flow as though they were siblings.
    ///
    /// `lora` rather than `radio` because this unit has three radios, so `radio` names none of them.
    ///
    /// `POST` and no body: there is nothing to say. The command carries no accessory type and no serial,
    /// because the device adopts whichever accessory is in its own pairing state — so this is a button and
    /// not a choice, and a body offering one would be a lie about what the protocol can express.
    ///
    /// Nothing has to close the window. The register clears itself when it ends.
    async fn pair_lora_accessory(Session { handle, .. }: Session) -> Response {
        // Worth a line of its own: for as long as the window is open the device will adopt an accessory
        // that asks to be adopted, and this is the only record that somebody opened it.
        tracing::info!("opening a pairing window on the LoRa radio");
        dispatch(&handle, Action::Send(Command::PairLoraAccessory)).await
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

    /// Everything the device has an accessory entry for, on either transport.
    ///
    /// **A list, not a meter.** The enrolment routes manage the one entry an mDNS search puts here; the
    /// device can hold others, reached by an address a server supplies, and those are reported with `kind`
    /// distinguishing them rather than hidden. Accessories the vendor binds through its cloud rather than
    /// through the device — its own smart plugs, of which there may be several — never appear in either
    /// list, so an empty reply is not a claim that nothing is attached.
    ///
    /// The routes are grouped `accessories/<transport>/<how it was acquired>/`, and the second segment is
    /// the same vocabulary as an entry's `kind`: one reading `discovered` is managed at
    /// `accessories/network/discovered/`. A `dialled` entry has no routes yet and
    /// `accessories/network/dialled/` is reserved for it. Both levels are needed — the transport alone
    /// would cover two mechanisms that share register 122 and leave the second nothing to be called, and
    /// the mechanism alone would put the radio's one-shot adoption on the same footing as a staged flow.
    ///
    /// Cached, so it costs no device traffic: both lists ride in the identity report. A `POST
    /// …/config/{key}/read` on the list register is how a caller asks for a fresh one.
    ///
    /// `in_use` is read from **telemetry**, not from the list, because the list cannot answer it: an
    /// accessory enrolled with `access` 1 appears in it identically to one in service, and only the meter
    /// reading says which. `manufacturer`, `model` and `access` come from the accessory's own report and are
    /// absent until one arrives — an accessory the device has never reached has none of them, which is
    /// itself worth seeing.
    async fn accessories<D: Catalogue + Enrols>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
    ) -> Response {
        let driver = state.driver.as_ref();
        let mut accessories = Vec::new();

        let reported = handle.accessory.borrow().clone();
        let in_use = Self::reading_is_set(&handle, driver.accessory_in_use_reading());

        for (transport, register) in [
            (Transport::Network, Some(driver.accessory_list())),
            (Transport::Lora, driver.accessory_list_radio()),
        ] {
            let Some(register) = register else { continue };
            let Some(value) = Self::cached_config(&handle, driver, register) else {
                continue;
            };
            accessories.extend(Self::describe_all(
                driver,
                transport,
                driver.enrolled(&value),
                reported.as_ref(),
                in_use,
            ));
        }

        axum::Json(serde_json::json!({ "accessories": accessories })).into_response()
    }

    /// Render decoded entries, filling in what the accessory's own report and telemetry can add.
    ///
    /// Shared so that a route which changes the list answers in the same shape as the one that reads it.
    fn describe_all<D: Catalogue + Enrols>(
        driver: &D,
        transport: Transport,
        entries: Vec<Enrolled>,
        reported: Option<&AccessoryView>,
        in_use: Option<bool>,
    ) -> Vec<AccessoryEntryView> {
        entries
            .into_iter()
            .map(|entry| {
                // Only the accessory the device is actually reporting about can be matched to a report: it
                // names one serial, and an entry with none cannot be it.
                let report = reported.filter(|report| {
                    entry
                        .serial
                        .is_some_and(|serial| driver.accessory_serial(&report.serial) == Some(serial))
                });
                AccessoryEntryView {
                    transport,
                    kind: entry.kind,
                    entry: entry.entry,
                    mode: entry.mode,
                    name: entry.name,
                    state: entry.state_label,
                    state_code: entry.state,
                    serial: entry.serial.map(|serial| serial.to_string()),
                    mac: entry.serial.map(|serial| driver.accessory_mac(serial)),
                    address: entry.address,
                    // Only meaningful for an entry the device is polling, and only knowable from telemetry.
                    in_use: entry.paired.then_some(in_use).flatten(),
                    manufacturer: report.map(|report| report.manufacturer.clone()),
                    model: report.map(|report| report.model.clone()),
                    access: report.map(|report| report.access),
                    communicating: report.map(|report| report.communicating),
                }
            })
            .collect()
    }

    /// Start a search for an accessory on the local network, streaming what the device finds.
    ///
    /// `{"model": "shelly-pro-3em"}`, or `{"service": "_http._tcp.", "type": 2}` to name both fields
    /// directly — the type is the *device's* index for a model and not the vendor's model code, so the
    /// table is what spares a caller knowing it.
    ///
    /// **A second search joins the first.** The window is one register on the device, so two cannot run
    /// independently; rather than refuse, this attaches to the window already open and streams its
    /// candidates. A search for a *different* service or type does start a new one, because streaming the
    /// results of something the caller did not ask for would be worse than restarting.
    ///
    /// ⚠ **Refused while an accessory is enrolled**, with `409`, and the refusal is load-bearing. A search
    /// over a live entry does **not** restart it: the device allocates a *second* entry, whose own search
    /// then finds nothing, and whose presence stops the paired accessory being polled until it is deleted.
    /// That is what the vendor's application runs into, and it reports it as a failed search.
    /// `{"replace": true}` says to delete the paired entry first and search anyway, which is the sequence
    /// that works — against a tombstone the same write revives it in place, keeping its number.
    async fn search_discovered<D: Catalogue + Enrols>(
        State(state): State<ApiState<D>>,
        Session { handle, device }: Session,
        body: Result<axum::Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        const EXPECTED: &str =
            r#"expected a body like {"model":"shelly-pro-3em"} or {"service":"_http._tcp.","type":2}"#;

        let driver = state.driver.as_ref();
        let Ok(axum::Json(body)) = body else {
            return problem(StatusCode::BAD_REQUEST, EXPECTED);
        };
        let target = match Self::search_target(driver, &body) {
            Ok(target) => target,
            Err(response) => return response,
        };
        let replace = body
            .get("replace")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Read the list rather than trust the cache: this decides whether a working accessory is about to
        // be reset, and a config write is not acknowledged, so the cache can lag a change by seconds.
        let enrolled = Self::read_enrolled(&handle, driver).await;
        if let Some(paired) = enrolled.iter().find(|entry| entry.paired) {
            if !replace {
                let named = paired
                    .serial
                    .map_or_else(|| "an accessory".to_owned(), |serial| driver.accessory_mac(serial));
                return problem(
                    StatusCode::CONFLICT,
                    &format!(
                        "{named} is enrolled and being polled; a search resets the entry it uses, so delete it \
                         first or repeat this with {{\"replace\":true}}"
                    ),
                );
            }
            tracing::info!(%device, entry = paired.entry, "replacing an enrolled accessory: deleting it first");
            let forget = Command::ForgetDiscoveredAccessory { entry: paired.entry };
            if handle.carry_out(Action::Send(forget)).await.is_err() {
                return problem(
                    StatusCode::BAD_GATEWAY,
                    "could not delete the enrolled accessory, so the search was not started",
                );
            }
        }

        let window = driver.accessory_search_window();
        let opened = state.searches.open(&device, &target, window);
        if opened.started {
            tracing::info!(%device, service = %target.0, accessory = target.1, "searching for a network accessory");
            let search = Command::DiscoverAccessories {
                service: target.0.clone(),
                accessory: target.1,
            };
            if handle.carry_out(Action::Send(search)).await.is_err() {
                state.searches.close(&device);
                return problem(StatusCode::BAD_GATEWAY, "the search could not be sent to the device");
            }
        } else {
            tracing::info!(%device, "joining a search already running");
        }

        let found_register = match driver.config_named(driver.accessory_found_register()) {
            Some(field) => field.register().number(),
            None => return problem(StatusCode::NOT_IMPLEMENTED, "this driver has no search-result register"),
        };
        Self::stream_candidates(handle, Arc::clone(&state.driver), device, found_register, opened.until)
    }

    /// Yield each accessory the device reports, once, until the window closes.
    ///
    /// **Only what the device *reports* inside the window counts.** The result register is not cleared —
    /// not by a pair, not by a delete — so its value can be a leftover from a search minutes ago, and
    /// emitting the cached value would invent a candidate nothing is offering. What separates the two is
    /// how a value arrived: the device volunteers this register only while a search is open, and nothing
    /// here reads it, so a report carrying it is this window's.
    fn stream_candidates<D: Catalogue + Enrols + Send + Sync + 'static>(
        handle: SessionHandle,
        driver: Arc<D>,
        device: String,
        found_register: u16,
        until: Instant,
    ) -> Response {
        let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(QUEUE_DEPTH);
        tokio::spawn(async move {
            let started = Instant::now();
            let mut identity = handle.identity.clone();
            let mut found: Vec<u64> = Vec::new();

            loop {
                // Cloned out of the borrow before any await: a held guard blocks every writer.
                let report = identity.borrow_and_update().clone();
                if let Some(report) = report {
                    let fresh = report.reported.contains(&found_register);
                    let value = fresh
                        .then(|| {
                            report
                                .entries
                                .iter()
                                .find(|entry| entry.register == found_register)
                                .map(|entry| entry.value.clone())
                        })
                        .flatten();
                    if let Some(serial) = value.and_then(|value| driver.accessory_found(&value))
                        && !found.contains(&serial)
                    {
                        {
                            found.push(serial);
                            let line = serde_json::json!({
                                "serial": serial.to_string(),
                                "mac": driver.accessory_mac(serial),
                            });
                            let mut line = line.to_string();
                            line.push('\n');
                            if tx.send(Ok(line)).await.is_err() {
                                // The caller left. The device goes on searching regardless — the window is
                                // its own, not this request's — so there is nothing to stop, only to stop
                                // watching.
                                tracing::debug!(%device, "a caller stopped reading an accessory search");
                                return;
                            }
                        }
                    }
                }

                let now = Instant::now();
                if now >= until {
                    break;
                }
                let wait = until.saturating_duration_since(now).min(Self::SEARCH_POLL);
                drop(tokio::time::timeout(wait, identity.changed()).await);
            }

            let duration = started.elapsed().as_secs();
            tracing::info!(%device, found = found.len(), duration_seconds = duration, "accessory search finished");
            let summary = serde_json::json!({ "found": found.len(), "duration_seconds": duration });
            let mut line = summary.to_string();
            line.push('\n');
            drop(tx.send(Ok(line)).await);
        });

        (
            [(http::header::CONTENT_TYPE, "application/jsonl")],
            axum::body::Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response()
    }

    /// Pair an accessory a search reported, and put its reading in service.
    ///
    /// `{"serial": "187723572702975"}` — the MAC is accepted too, in any usual notation. `access` defaults
    /// to `0`, which is what makes the device *use* the reading; passing `1` enrols an accessory that is
    /// polled and answers while the device's own meter registers stay zero, which is a real thing to want
    /// and a terrible thing to get by accident.
    ///
    /// **The device decides whether the serial means anything.** A pair command with no search behind it
    /// changes nothing at all, and this does not try to predict that: checking would mean keeping a record
    /// of every serial ever reported and would still be a guess, since the device's own result register is
    /// cleared unpredictably. The reply reports what the device did — the entry's state, and whether the
    /// reading is in use — which is the same answer arrived at without a second opinion.
    ///
    /// **`in_use` costs a wait.** The device begins polling within about ten seconds and the reading
    /// follows on the next telemetry cycle, so this waits for it rather than returning a body whose one
    /// interesting field is empty. `false` after the wait means "not yet, or never" — re-read
    /// `GET …/accessories` rather than believe either — and `null` means the accessory is not in the
    /// device's list, which is what a serial it never found looks like.
    async fn pair_discovered<D: Catalogue + Enrols>(
        State(state): State<ApiState<D>>,
        Session { handle, device }: Session,
        body: Result<axum::Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        const EXPECTED: &str = r#"expected a body like {"serial":"187723572702975","access":0}"#;

        let driver = state.driver.as_ref();
        let Ok(axum::Json(body)) = body else {
            return problem(StatusCode::BAD_REQUEST, EXPECTED);
        };
        let Some(offered) = body.get("serial").and_then(Self::as_text) else {
            return problem(StatusCode::BAD_REQUEST, EXPECTED);
        };
        let Some(serial) = driver.accessory_serial(&offered) else {
            return problem(
                StatusCode::BAD_REQUEST,
                &format!("{offered:?} is not an accessory serial or MAC"),
            );
        };
        let access = match body.get("access") {
            None => 0,
            Some(value) => match value.as_u64().and_then(|value| u16::try_from(value).ok()) {
                Some(access) => access,
                None => return problem(StatusCode::BAD_REQUEST, "access must be a small whole number"),
            },
        };

        tracing::info!(%device, serial, access, in_service = access == 0, "pairing a network accessory");
        let command = Command::PairDiscoveredAccessory { serial, access };
        if handle.carry_out(Action::Send(command)).await.is_err() {
            return problem(
                StatusCode::BAD_GATEWAY,
                "the pair command could not be sent to the device",
            );
        }

        let waited = Self::await_in_use(&handle, driver, Self::PAIR_SETTLE).await;
        let enrolled = Self::read_enrolled(&handle, driver).await;
        let entry = enrolled.into_iter().find(|entry| entry.serial == Some(serial));

        axum::Json(serde_json::json!({
            "serial": serial.to_string(),
            "mac": driver.accessory_mac(serial),
            "access": access,
            "state": entry.as_ref().map(|entry| entry.state_label),
            "state_code": entry.as_ref().map(|entry| entry.state),
            "address": entry.as_ref().and_then(|entry| entry.address.clone()),
            // Null rather than false when the accessory is not in the list at all: the reading being in use
            // is a fact about the device's meter, and reporting it for an accessory the device never
            // enrolled would answer a different question than the one asked.
            "in_use": entry.as_ref().map(|_| waited.0),
            "waited_seconds": waited.1.as_secs(),
        }))
        .into_response()
    }

    /// One enrolled accessory, by the number its entry carries.
    ///
    /// The number comes from `GET …/accessories`, where every entry reports it. It is the device's own and
    /// cannot be predicted, so this is always a read-then-address operation.
    async fn discovered_one<D: Catalogue + Enrols>(
        State(state): State<ApiState<D>>,
        Session { handle, .. }: Session,
        Entry(entry): Entry,
    ) -> Response {
        let driver = state.driver.as_ref();
        let Some(value) = Self::cached_config(&handle, driver, driver.accessory_list()) else {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "the device has not reported its accessories yet",
            );
        };
        let found = driver.enrolled(&value).into_iter().find(|one| one.entry == entry);
        let Some(found) = found else {
            return problem(
                StatusCode::NOT_FOUND,
                &format!("no accessory entry {entry} on this device"),
            );
        };

        let in_use = Self::reading_is_set(&handle, driver.accessory_in_use_reading());
        let reported = handle.accessory.borrow().clone();
        let mut described = Self::describe_all(driver, Transport::Network, vec![found], reported.as_ref(), in_use);
        match described.pop() {
            Some(view) => axum::Json(view).into_response(),
            None => problem(
                StatusCode::NOT_FOUND,
                &format!("no accessory entry {entry} on this device"),
            ),
        }
    }

    /// Remove one enrolled accessory, by the number its entry carries.
    ///
    /// **The number is the whole selector.** The device's delete names it and reads nothing else — a
    /// command carrying a service never browsed and an index never requested removes the named entry just
    /// the same. With one entry in the list any number that matched it worked, which is how this was first
    /// written as taking nothing at all; a device holding two showed that wrong.
    ///
    /// The number cannot be guessed. It is assigned by the device — a second entry registered while the
    /// first was live came back as `112` — so a caller reads it from `GET …/accessories` and passes it
    /// here.
    ///
    /// The reply is the list as it reads afterwards, so a caller sees what the device did.
    async fn forget_discovered<D: Catalogue + Enrols>(
        State(state): State<ApiState<D>>,
        Session { handle, device }: Session,
        Entry(entry): Entry,
    ) -> Response {
        let driver = state.driver.as_ref();

        // Taken before the write, so the read-back can tell the new value from the old one.
        let before = Self::read_enrolled(&handle, driver).await;
        if !before.iter().any(|one| one.entry == entry) {
            return problem(
                StatusCode::NOT_FOUND,
                &format!("no accessory entry {entry} on this device"),
            );
        }

        tracing::info!(%device, entry, "deleting an enrolled accessory");
        let command = Command::ForgetDiscoveredAccessory { entry };
        if handle.carry_out(Action::Send(command)).await.is_err() {
            return problem(StatusCode::BAD_GATEWAY, "the delete could not be sent to the device");
        }

        let entries = Self::read_enrolled_after(&handle, driver, &before).await;
        let in_use = Self::reading_is_set(&handle, driver.accessory_in_use_reading());
        let reported = handle.accessory.borrow().clone();
        let accessories = Self::describe_all(driver, Transport::Network, entries, reported.as_ref(), in_use);
        axum::Json(serde_json::json!({
            "accessories": accessories,
            "detail": "a delete tombstones the entry rather than removing it; a later search revives it in \
                       place, keeping its number",
        }))
        .into_response()
    }

    /// The service and type a search body names, or the problem to answer with.
    #[expect(
        clippy::result_large_err,
        reason = "the error is a ready-made HTTP response; boxing it would only move the refusal's own body"
    )]
    fn search_target<D: Catalogue + Enrols>(driver: &D, body: &serde_json::Value) -> Result<(String, u16), Response> {
        let model = body.get("model").and_then(serde_json::Value::as_str);
        let service = body.get("service").and_then(serde_json::Value::as_str);
        let accessory = body
            .get("type")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());

        match (model, service, accessory) {
            (Some(model), None, None) => driver.accessory_search(model).ok_or_else(|| {
                problem(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "{model:?} is not a model this build knows; it knows {}, or name the service and type \
                         directly",
                        driver.accessory_models().join(", ")
                    ),
                )
            }),
            (None, Some(service), Some(accessory)) => Ok((service.to_owned(), accessory)),
            (Some(_), _, _) => Err(problem(
                StatusCode::BAD_REQUEST,
                "name either a model or a service and type, not both",
            )),
            _ => Err(problem(
                StatusCode::BAD_REQUEST,
                "say what to look for: a model, or a service and type",
            )),
        }
    }

    /// A JSON value as text, accepting a number for a field whose values are digits.
    ///
    /// A serial is a decimal, so a caller writing it unquoted is making a reasonable mistake — and one
    /// large enough to lose precision in some JSON encoders, which is why it is documented as a string.
    fn as_text(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    /// One cached config register by name, as the device last reported it.
    fn cached_config<D: Catalogue>(handle: &SessionHandle, driver: &D, name: &str) -> Option<String> {
        let register = driver.config_named(name)?.register().number();
        let report = handle.identity.borrow();
        report
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.register == register)
            .map(|entry| entry.value.clone())
    }

    /// Read the accessory list back from the device, falling back to the cache.
    ///
    /// A config write draws no acknowledgement and its effect arrives as a separate report, so a read
    /// issued immediately after one returns the *previous* value. Everything here that reports what the
    /// device did goes through this.
    async fn read_enrolled<D: Catalogue + Enrols>(handle: &SessionHandle, driver: &D) -> Vec<Enrolled> {
        Self::read_config_back(handle, driver, driver.accessory_list())
            .await
            .map(|value| driver.enrolled(&value))
            .unwrap_or_default()
    }

    /// Ask the device for one config register and wait for it to report it.
    ///
    /// Falls back to the cache: that is what the device last said, and saying nothing at all would be less
    /// true than saying that.
    async fn read_config_back<D: Catalogue>(handle: &SessionHandle, driver: &D, name: &str) -> Option<String> {
        let field = driver.config_named(name)?;
        let register = field.register();
        let mut identity = handle.identity.clone();
        identity.mark_unchanged();
        drop(
            handle
                .carry_out(Action::Send(Command::ReadConfig {
                    registers: vec![register],
                }))
                .await,
        );

        let deadline = Instant::now().checked_add(Self::READ_BACK);
        loop {
            let report = identity.borrow_and_update().clone();
            if let Some(report) = report
                && report.reported.contains(&register.number())
                && let Some(entry) = report.entries.iter().find(|entry| entry.register == register.number())
            {
                return Some(entry.value.clone());
            }
            let now = Instant::now();
            let Some(deadline) = deadline.filter(|end| now < *end) else {
                break;
            };
            drop(tokio::time::timeout(deadline.saturating_duration_since(now), identity.changed()).await);
        }

        Self::cached_config(handle, driver, name)
    }

    /// Read the accessory list back until it differs from what it was, or the wait runs out.
    ///
    /// A config write draws no acknowledgement, and the device answers a read issued straight afterwards
    /// with the value it held *before* the write — verified: a delete answered `paired` for eight seconds
    /// and then went to `deleted`. Waiting for a report is therefore not enough; what a caller needs is a
    /// report that is not the old one. This asks again, paced, until the value moves.
    ///
    /// It gives up rather than failing: an unchanged list after the wait is a real possibility — the device
    /// may have ignored the command — and reporting what it actually says is more use than an error.
    async fn read_enrolled_after<D: Catalogue + Enrols>(
        handle: &SessionHandle,
        driver: &D,
        before: &[Enrolled],
    ) -> Vec<Enrolled> {
        let deadline = Instant::now().checked_add(Self::READ_BACK);
        let mut latest = Self::read_enrolled(handle, driver).await;
        while latest == before {
            let now = Instant::now();
            if deadline.is_none_or(|end| now >= end) {
                break;
            }
            tokio::time::sleep(Self::READ_BACK_PACE).await;
            latest = Self::read_enrolled(handle, driver).await;
        }
        latest
    }

    /// Whether a telemetry reading is currently non-zero, or `None` if no frame has carried it.
    fn reading_is_set(handle: &SessionHandle, name: &str) -> Option<bool> {
        let telemetry = handle.telemetry.borrow();
        telemetry
            .as_ref()?
            .readings
            .iter()
            .find(|reading| reading.name == name)
            .map(|reading| reading.raw != 0)
    }

    /// Wait for the device to start using an accessory's reading, and say how long it took.
    async fn await_in_use<D: Catalogue + Enrols>(
        handle: &SessionHandle,
        driver: &D,
        limit: Duration,
    ) -> (bool, Duration) {
        let name = driver.accessory_in_use_reading();
        let started = Instant::now();
        let mut telemetry = handle.telemetry.clone();
        loop {
            if Self::reading_is_set(handle, name) == Some(true) {
                return (true, started.elapsed());
            }
            let waited = started.elapsed();
            if waited >= limit {
                return (false, waited);
            }
            drop(tokio::time::timeout(limit.saturating_sub(waited), telemetry.changed()).await);
        }
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
    use super::{Api, Duration, Outcome, Registry, Searches, SessionHandle, SettingView, StatusView, resolve};
    use crate::driver::catalogue::Catalogue as _;
    use crate::growatt::driver::Growatt;
    use crate::model::{Raw, Register};
    use std::sync::Arc;
    use tokio::sync::Notify;

    /// The device in these tests, matching the serial used across the documentation.
    const DEVICE: &str = "0EXAMPLE00000001";

    #[tokio::test(start_paused = true)]
    async fn a_second_search_for_the_same_thing_joins_the_first_rather_than_restarting_it() {
        // The device's search is one register, so two cannot run independently. Restarting under the first
        // caller would cut their window short; refusing would be a conflict where there is none, since the
        // register the second caller wants to read is the one already filling.
        let searches = Searches::default();
        let target = ("_http._tcp.".to_owned(), 2);
        let window = Duration::from_mins(1);

        let first = searches.open(DEVICE, &target, window);
        assert!(first.started, "nothing was open, so this one starts it");

        let second = searches.open(DEVICE, &target, window);
        assert!(!second.started, "no second search command may be sent");
        assert_eq!(
            second.until, first.until,
            "and it ends when the first does, not a minute later"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_for_something_else_starts_its_own_window() {
        // Streaming the results of a service the caller did not ask for would be worse than restarting.
        let searches = Searches::default();
        let window = Duration::from_mins(1);
        let first = searches.open(DEVICE, &("_http._tcp.".to_owned(), 2), window);
        let other = searches.open(DEVICE, &("_everhome._tcp.".to_owned(), 3), window);
        assert!(other.started);
        assert!(other.until >= first.until);
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_that_has_run_out_is_opened_again() {
        let searches = Searches::default();
        let target = ("_http._tcp.".to_owned(), 2);
        let window = Duration::from_mins(1);
        assert!(searches.open(DEVICE, &target, window).started);

        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(
            searches.open(DEVICE, &target, window).started,
            "the device stopped searching a second ago, so this is a new one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_that_could_not_be_sent_leaves_no_window_behind() {
        let searches = Searches::default();
        let target = ("_http._tcp.".to_owned(), 2);
        let window = Duration::from_mins(1);
        assert!(searches.open(DEVICE, &target, window).started);
        searches.close(DEVICE);
        assert!(
            searches.open(DEVICE, &target, window).started,
            "a window nothing opened on the device must not be joined"
        );
    }

    #[test]
    fn a_search_body_names_a_model_or_the_two_fields_and_not_both() {
        let by_model = serde_json::json!({"model": "shelly-pro-3em"});
        assert_eq!(
            Api::search_target(&Growatt, &by_model).ok(),
            Some(("_http._tcp.".to_owned(), 2))
        );

        let direct = serde_json::json!({"service": "_everhome._tcp.", "type": 3});
        assert_eq!(
            Api::search_target(&Growatt, &direct).ok(),
            Some(("_everhome._tcp.".to_owned(), 3))
        );

        // A model the table does not carry is a refusal, not a guess: the type is the device's own index.
        assert!(Api::search_target(&Growatt, &serde_json::json!({"model": "homewizard-p1"})).is_err());
        assert!(Api::search_target(&Growatt, &serde_json::json!({})).is_err());
        assert!(
            Api::search_target(&Growatt, &serde_json::json!({"model": "shelly-pro-3em", "type": 2})).is_err(),
            "naming both leaves it ambiguous which the caller meant"
        );
    }

    #[test]
    fn a_serial_may_be_written_as_a_number_where_a_string_is_documented() {
        // A serial is all digits, so writing it unquoted is a reasonable mistake to make.
        assert_eq!(
            Api::as_text(&serde_json::json!("187723572702975")).as_deref(),
            Some("187723572702975")
        );
        assert_eq!(
            Api::as_text(&serde_json::json!(187_723_572_702_975_u64)).as_deref(),
            Some("187723572702975")
        );
        assert_eq!(Api::as_text(&serde_json::json!(null)), None);
    }

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
                accessory: tokio::sync::watch::channel(None).1,
                stop: Arc::new(Notify::new()),
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
        // Session ids come from one counter shared by every device, so a device's own ids are not
        // contiguous. What must hold is that each entry is owned by the registration that inserted it,
        // whatever numbers the others consumed in between.
        let registry = Registry::new();
        let (a1, _ra1, _sa1) = handle();
        let (b1, _rb1, _sb1) = handle();
        let (a2, _ra2, _sa2) = handle();

        let a_old = registry.register("0EXAMPLE0000000A", a1);
        let _b = registry.register("0EXAMPLE0000000B", b1);
        let _a_new = registry.register("0EXAMPLE0000000A", a2);

        // A's replacement took session 2, with B's registration holding 1 in between.
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
    fn a_reconnect_is_announced_although_the_same_devices_are_connected_before_and_after() {
        // What a subscriber cannot work out for itself. The same one device is connected throughout, so a
        // set of serials compares equal and nothing is published — while anything holding the replaced
        // session's channels goes on holding channels nobody writes to. The Home Assistant link did
        // exactly that, and stayed on the dead session until its socket timed out ten minutes later.
        let registry = Registry::new();
        let mut watch = registry.watch();

        let (first, _rx1, _s1) = handle();
        let _old = registry.register("0EXAMPLE00000001", first);
        watch.borrow_and_update();

        let (second, _rx2, _s2) = handle();
        let _new = registry.register("0EXAMPLE00000001", second);
        assert!(
            watch.has_changed().expect("the sender outlives this"),
            "the session behind the serial changed, which is the only thing that did"
        );
        let connected = watch.borrow_and_update().clone();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected.devices().collect::<Vec<&str>>(), vec!["0EXAMPLE00000001"]);
    }

    #[test]
    fn a_session_is_found_with_the_identity_of_the_registration_serving_it() {
        // Both from one lock, so a caller cannot pair a serial with a session that no longer serves it.
        let registry = Registry::new();
        let (first, _rx1, _s1) = handle();
        let _old = registry.register("0EXAMPLE00000001", first);
        let (before, _) = registry.session("0EXAMPLE00000001").expect("a session");

        let (second, _rx2, _s2) = handle();
        let _new = registry.register("0EXAMPLE00000001", second);
        let (after, _) = registry.session("0EXAMPLE00000001").expect("a session");

        assert_ne!(before, after, "a replacement is a different session");
        assert!(registry.session("0EXAMPLE00000002").is_none());
    }

    #[tokio::test]
    async fn a_replaced_session_is_told_to_stop() {
        // A displaced session cannot discover this for itself: its socket is half-open, so reads stop
        // rather than fail, while its cloud relay goes on taking commands and writing them to a connection
        // that is gone. One vendor-app meter search was swallowed exactly that way.
        let registry = Registry::new();
        let (first, _rx1, _s1) = handle();
        let stop = Arc::clone(&first.stop);
        let _old = registry.register("0EXAMPLE00000001", first);

        // Nothing has displaced it yet.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stop.notified())
                .await
                .is_err(),
            "a session that is still current must not be told to stop"
        );

        let (second, _rx2, _s2) = handle();
        let live = Arc::clone(&second.stop);
        let _new = registry.register("0EXAMPLE00000001", second);

        assert!(
            tokio::time::timeout(Duration::from_millis(200), stop.notified())
                .await
                .is_ok(),
            "the displaced session must be told"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), live.notified())
                .await
                .is_err(),
            "and the session that replaced it must not be"
        );
    }

    #[tokio::test]
    async fn registering_a_second_device_stops_nothing() {
        // The signal is per device. A second inverter arriving must not end the first one's session.
        let registry = Registry::new();
        let (first, _rx1, _s1) = handle();
        let stop = Arc::clone(&first.stop);
        let _a = registry.register("0EXAMPLE00000001", first);

        let (second, _rx2, _s2) = handle();
        let _b = registry.register("0EXAMPLE00000002", second);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), stop.notified())
                .await
                .is_err(),
            "another device's registration says nothing about this one"
        );
    }

    #[test]
    fn a_replaced_registration_going_away_publishes_nothing() {
        // The replacement is announced, and announced once: the old registration owns no entry by then,
        // so its going away — which can be minutes later, when a half-open socket finally times out —
        // says nothing about a device that is connected and being served.
        let registry = Registry::new();
        let (first, _rx1, _s1) = handle();
        let old = registry.register("0EXAMPLE00000001", first);

        let mut watch = registry.watch();
        watch.borrow_and_update();

        let (second, _rx2, _s2) = handle();
        let _new = registry.register("0EXAMPLE00000001", second);
        assert!(
            watch.has_changed().expect("the sender outlives this"),
            "a new session behind the same serial is the change, and this is the only word of it"
        );
        watch.borrow_and_update();

        drop(old);
        assert!(
            !watch.has_changed().expect("the sender outlives this"),
            "the replaced registration owned no entry, so nothing changed when it went away"
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
