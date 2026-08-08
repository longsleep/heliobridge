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

use core::time::Duration;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{FromRequestParts, RawPathParams, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::growatt::v7::encode::Command;
use crate::growatt::v7::registers::{HoldingRegister, SLOT_COUNT};
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
}

impl Outcome {
    /// An outcome for a register the device did not answer about.
    pub fn timed_out(register: Register, requested: Option<Raw>) -> Self {
        Self {
            name: HoldingRegister::lookup(register).map(|entry| entry.name),
            register: register.number(),
            requested: requested.map(Raw::get),
            stored: None,
            value: None,
            confirmed: false,
            error: Some("the device did not answer the read-back".to_owned()),
        }
    }

    /// An outcome from a value read back off the device.
    pub fn read_back(register: Register, requested: Option<Raw>, stored: Raw) -> Self {
        let entry = HoldingRegister::lookup(register);
        Self {
            name: entry.map(|entry| entry.name),
            register: register.number(),
            requested: requested.map(Raw::get),
            stored: Some(stored.get()),
            value: entry.map(|entry| entry.decode(stored).to_string()),
            // Nothing requested means nothing to disagree with, so learning the value is success.
            confirmed: requested.is_none_or(|wanted| wanted == stored),
            error: None,
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
    /// Describe one register's stored value, or `None` if it has no documented meaning.
    pub fn new(register: Register, raw: Raw) -> Option<Self> {
        let entry = HoldingRegister::lookup(register)?;
        Some(Self {
            register: register.number(),
            name: entry.name,
            raw: raw.get(),
            value: entry.decode(raw).to_string(),
            unit: entry.unit.symbol(),
        })
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
    /// Documented field name, or `null` for a key this build cannot name.
    pub name: Option<&'static str>,
    /// What the field is for: identity, metadata, dynamic, endpoint, inert, or `null` when unknown.
    pub role: Option<&'static str>,
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

/// How the API reaches one device's session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    /// Requests for that session to carry out.
    pub requests: mpsc::Sender<Request>,
    /// Its current settings, so a read needs no device traffic.
    pub settings: watch::Receiver<Vec<SettingView>>,
    /// What the datalogger last said about itself. Absent until the first report, about a second in.
    pub identity: watch::Receiver<Option<IdentityView>>,
    /// The most recent telemetry frame. Absent until the first one arrives, about a second in.
    pub telemetry: watch::Receiver<Option<TelemetryView>>,
}

/// Which devices are connected, and how to reach each.
///
/// Shared between the API and every session. A session registers itself once its serial is known, and
/// removes itself when it ends — by [`Registration`]'s `Drop`, so it happens on the error paths too.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

/// The registry's contents: each device's handle, tagged with which registration owns it.
#[derive(Debug, Default)]
struct Inner {
    devices: HashMap<String, (u64, SessionHandle)>,
    next_epoch: u64,
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
    /// Each registration carries an epoch, and dropping one removes the entry **only if it is still the
    /// current one**. Without that, the ordering on a reconnect — new session registers, then the old
    /// session's guard drops — would delete the live entry and leave a connected device unaddressable.
    pub fn register(&self, device_id: &str, handle: SessionHandle) -> Registration {
        let epoch = match self.inner.lock() {
            Ok(mut inner) => {
                let epoch = inner.next_epoch;
                inner.next_epoch = inner.next_epoch.saturating_add(1);
                inner.devices.insert(device_id.to_owned(), (epoch, handle));
                epoch
            }
            Err(_) => 0,
        };

        Registration {
            registry: self.clone(),
            device_id: device_id.to_owned(),
            epoch,
        }
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
    epoch: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.registry.inner.lock() {
            // Only if this registration is still the current one for the device.
            if inner
                .devices
                .get(&self.device_id)
                .is_some_and(|(epoch, _)| *epoch == self.epoch)
            {
                inner.devices.remove(&self.device_id);
            }
        }
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
pub fn listen(path: &Path, registry: Registry) -> Result<(), ControlError> {
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
        .route("/devices", get(Api::devices))
        .route("/devices/{device}/identity", get(Api::identity))
        .route("/devices/{device}/telemetry", get(Api::telemetry))
        .route("/devices/{device}/telemetry/{key}", get(Api::reading))
        .route("/devices/{device}/settings", get(Api::settings))
        .route("/devices/{device}/settings/{key}", get(Api::setting).put(Api::write))
        .route("/devices/{device}/settings/{key}/read", post(Api::refresh))
        .with_state(registry);

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
struct Session(SessionHandle);

impl FromRequestParts<Registry> for Session {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, registry: &Registry) -> Result<Self, Self::Rejection> {
        let device = path_param(parts, "device").await?;
        registry.handle(&device).map(Self).ok_or_else(|| {
            Rejection::new(
                StatusCode::NOT_FOUND,
                format!("no connected device {device:?}; see /devices"),
            )
        })
    }
}

/// The `{key}` path segment, whatever it names.
///
/// Telemetry fields are not holding registers, so a reading is found by name rather than resolved to a
/// writable register. This carries the segment as sent.
struct Key(String);

impl FromRequestParts<Registry> for Key {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, _registry: &Registry) -> Result<Self, Self::Rejection> {
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

impl FromRequestParts<Registry> for Setting {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, _registry: &Registry) -> Result<Self, Self::Rejection> {
        let key = path_param(parts, "key").await?;
        let register =
            resolve(&key).ok_or_else(|| Rejection::new(StatusCode::NOT_FOUND, format!("unknown setting {key:?}")))?;
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
    async fn devices(State(registry): State<Registry>) -> Response {
        axum::Json(serde_json::json!({ "devices": registry.devices() })).into_response()
    }

    /// What the datalogger says about itself: firmware, model, network, clock, endpoint.
    ///
    /// Every field it reported, the serial and password included. From the report sent on every connect, so
    /// no device traffic.
    async fn identity(Session(handle): Session) -> Response {
        Self::cached(
            handle.identity.borrow().clone(),
            "no identity report yet; the device sends one on connect",
        )
    }

    /// The most recent telemetry frame, every register it carried.
    async fn telemetry(Session(handle): Session) -> Response {
        Self::cached(
            handle.telemetry.borrow().clone(),
            "no telemetry yet; the device publishes about a second after connecting",
        )
    }

    /// One telemetry reading, by field name or register number.
    async fn reading(Session(handle): Session, Key(key): Key) -> Response {
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

    /// Every setting a device's session knows, from its cache. No device traffic.
    async fn settings(Session(handle): Session) -> Response {
        axum::Json(handle.settings.borrow().clone()).into_response()
    }

    /// One setting from the cache.
    async fn setting(Session(handle): Session, setting: Setting) -> Response {
        let found = handle
            .settings
            .borrow()
            .iter()
            .find(|view| view.register == setting.register.number())
            .cloned();

        match found {
            Some(view) => axum::Json(view).into_response(),
            // Known register, no value yet: the startup read-back has not reached it, which is a different
            // thing from it not existing.
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
        Session(handle): Session,
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
        let command = match Command::set(setting.register, body.value) {
            Ok(command) => command,
            Err(error) => return problem(StatusCode::BAD_REQUEST, &error.to_string()),
        };

        dispatch(&handle, Action::Apply(command)).await
    }

    /// Force a read of one register.
    async fn refresh(Session(handle): Session, setting: Setting) -> Response {
        dispatch(&handle, Action::Refresh(setting.register)).await
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

/// Hand an action to a session and wait for its outcome.
async fn dispatch(handle: &SessionHandle, action: Action) -> Response {
    let (reply, answer) = oneshot::channel();
    if handle.requests.try_send(Request { action, reply }).is_err() {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "the session's command queue is full");
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
        Ok(Ok(outcome)) => {
            let code = if outcome.confirmed {
                StatusCode::OK
            } else {
                // The request was carried out; the device simply did not do what was asked. 409 says that
                // more precisely than either 200 or 500.
                StatusCode::CONFLICT
            };
            (code, axum::Json(outcome)).into_response()
        }
        Ok(Err(_)) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "the device session ended before answering",
        ),
        Err(_) => problem(
            StatusCode::GATEWAY_TIMEOUT,
            &format!("no answer within {}s", REQUEST_TIMEOUT.as_secs()),
        ),
    }
}

/// Accept either a field name or a register number.
///
/// Names are what the specification uses and what a person will type; numbers are what the protocol uses
/// and what a script may already hold.
fn resolve(key: &str) -> Option<Register> {
    if let Ok(number) = key.parse::<u16>() {
        return Some(Register(number));
    }
    HoldingRegister::resync_set(SLOT_COUNT)
        .into_iter()
        .find(|entry| entry.name == key)
        .map(|entry| entry.register)
}

/// A JSON error body, so a script does not have to parse prose.
fn problem(code: StatusCode, detail: &str) -> Response {
    (code, axum::Json(serde_json::json!({ "error": detail }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Registry, SessionHandle, SettingView, resolve};
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
        (
            SessionHandle {
                requests: requests_tx,
                settings: settings_rx,
                identity: identity_rx,
                telemetry: telemetry_rx,
            },
            requests_rx,
            settings_tx,
        )
    }

    #[test]
    fn settings_resolve_by_name_or_number() {
        assert_eq!(resolve("slot1_output_power"), Some(Register(257)));
        assert_eq!(resolve("grid_power_allowed"), Some(Register(326)));
        assert_eq!(resolve("326"), Some(Register(326)));
        // A number is taken at face value even if undocumented; the encoder decides whether it may be
        // written, and reading anything is harmless.
        assert_eq!(resolve("321"), Some(Register(321)));
        assert_eq!(resolve("nonsense"), None);
        // Slots beyond the first resolve too, so a nine-slot install is addressable.
        assert_eq!(resolve("slot9_output_power"), Some(Register(297)));
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
    fn a_matching_read_back_confirms() {
        let outcome = Outcome::read_back(Register(257), Some(Raw(100)), Raw(100));
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
        let outcome = Outcome::read_back(Register(322), Some(Raw(1000)), Raw(800));
        assert!(!outcome.confirmed);
        assert_eq!(outcome.requested, Some(1000));
        assert_eq!(outcome.stored, Some(800));
    }

    #[test]
    fn learning_a_value_nobody_requested_counts_as_success() {
        // Reading 322 after toggling power_plus: no expected value, only a stale one to replace.
        let outcome = Outcome::read_back(Register(322), None, Raw(800));
        assert!(outcome.confirmed);
        assert_eq!(outcome.requested, None);
        assert_eq!(outcome.stored, Some(800));
    }

    #[test]
    fn an_unanswered_read_back_is_an_error_not_a_confirmation() {
        let outcome = Outcome::timed_out(Register(257), Some(Raw(100)));
        assert!(!outcome.confirmed);
        assert!(outcome.stored.is_none());
        assert!(outcome.error.is_some());
    }

    #[test]
    fn a_setting_view_renders_by_domain() {
        let flag = SettingView::new(Register(326), Raw(1)).expect("documented");
        assert_eq!(flag.name, "grid_power_allowed");
        assert_eq!(flag.value, "1");

        let time = SettingView::new(Register(254), Raw(0x173B)).expect("documented");
        assert_eq!(time.value, "23:59");

        let mode = SettingView::new(Register(256), Raw(2)).expect("documented");
        assert_eq!(mode.value, "smart_self_use");

        assert_eq!(
            SettingView::new(Register(257), Raw(100)).map(|view| view.unit),
            Some("W")
        );

        // Undocumented registers have nothing to show.
        assert!(SettingView::new(Register(321), Raw(0)).is_none());
    }

    #[test]
    fn outcomes_serialise_for_a_script_to_read() {
        let json =
            serde_json::to_string(&Outcome::read_back(Register(257), Some(Raw(100)), Raw(100))).expect("serialise");
        assert!(json.contains(r#""confirmed":true"#), "{json}");
        assert!(json.contains(r#""name":"slot1_output_power""#), "{json}");
        assert!(json.contains(r#""stored":100"#), "{json}");
    }
}
