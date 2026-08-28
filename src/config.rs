//! Configuration, entirely from the environment.
//!
//! `clap` derive gives the environment variables, `--help`, validation and defaults from one
//! definition. Every variable is prefixed `HELIOBRIDGE_`.

use core::time::Duration;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::growatt::cloud::{self, CloudConfig};
use crate::growatt::policy::{Answers, Mode, Policy};
use crate::homeassistant::command::Permitted;
use crate::homeassistant::publisher::{self, PublisherOptions};
use crate::homeassistant::topics::{self, Topics};
use crate::record::{self, RecorderConfig};
use crate::server::access::{Devices, Peers};

/// Default device-facing listener address.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:7006";

/// Directory name used under the temporary directory when no state directory is given.
pub const STATE_DIR_NAME: &str = "heliobridge";

/// Where generated state goes when nothing says otherwise.
///
/// Under the system temporary directory, so a first run works with no directory to create and no privilege
/// to acquire. What lives there is the generated certificate, which the device does not verify and which is
/// regenerated whenever it is missing — losing it costs a reconnect, not a reprovisioning.
///
/// A deployment that wants it kept says so: `HELIOBRIDGE_STATE_DIR=/var/lib/heliobridge`.
pub fn default_state_dir() -> PathBuf {
    std::env::temp_dir().join(STATE_DIR_NAME)
}

/// Default root for this program's own topics.
pub const DEFAULT_MQTT_BASE: &str = "heliobridge";

/// Default root Home Assistant watches for discovery.
pub const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";

/// Default tracing filter.
pub const DEFAULT_LOG: &str = "info";

/// What to do instead of serving.
///
/// Absent means serve, so the ordinary invocation is unchanged and every flag below still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum Command {
    /// Ask a running heliobridge whether it is alive, and exit 0 if it is.
    ///
    /// Probes the address in `--listen`, which is the same setting the server reads — so inside a
    /// container this needs no arguments. Reports only that the server is serving: a device is expected
    /// to be absent for hours at a time, and a health signal that said otherwise would call a working
    /// deployment broken every night.
    Healthz,
}

/// A local MQTT bridge for the Growatt Nexa 2000.
#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// What to do instead of serving.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Device-facing TLS listener.
    ///
    /// Port 7006 is where the device connects; changing it only makes sense alongside a destination
    /// NAT rule that rewrites the port.
    #[arg(long, env = "HELIOBRIDGE_LISTEN", default_value = DEFAULT_LISTEN)]
    pub listen: SocketAddr,

    /// PEM certificate to present to the device. Generated on first run if unset.
    #[arg(long, env = "HELIOBRIDGE_TLS_CERT", value_parser = path_allowing_empty)]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key matching `--tls-cert`.
    #[arg(long, env = "HELIOBRIDGE_TLS_KEY", value_parser = path_allowing_empty)]
    pub tls_key: Option<PathBuf>,

    /// Where to keep the generated certificate and cached state.
    ///
    /// Defaults under the system temporary directory, which survives restarts of this program but not
    /// always a reboot. Point it somewhere durable to keep one certificate across reboots.
    #[arg(long, env = "HELIOBRIDGE_STATE_DIR", default_value_os_t = default_state_dir(), value_parser = path_allowing_empty)]
    pub state_dir: PathBuf,

    /// Addresses and networks the device may connect from. Empty accepts any.
    ///
    /// Comma-separated, IPv4 and IPv6, addresses or networks:
    /// `192.168.2.238,192.168.2.0/24,2001:db8::/32`. A connection from anywhere else is dropped before the
    /// TLS handshake. An entry that cannot be read is a startup failure rather than one that matches
    /// nothing, since the failure mode of a mistyped list is a device that silently stops connecting.
    ///
    /// Listing one address family does not deny the other — a list says what is allowed, and a family
    /// nobody mentioned is simply absent from it. Loopback is not implicit either: list `127.0.0.1` and
    /// `::1` if something local has to reach the device listener. The control socket is unaffected, being a
    /// Unix socket rather than a network listener.
    #[arg(long, env = "HELIOBRIDGE_ALLOW_FROM", default_value = "")]
    pub allow_from: String,

    /// Device serials to serve. Empty serves any.
    ///
    /// Comma-separated. Checked at CONNECT, before the session registers, so a device that is not listed
    /// never reaches the control API, never gets a Home Assistant entity and never has a frame recorded. It
    /// is answered with a CONNACK refusal rather than a dropped socket, so the reason is visible in a
    /// capture.
    #[arg(long, env = "HELIOBRIDGE_ALLOW_DEVICES", default_value = "")]
    pub allow_devices: String,

    /// Record every frame here for later analysis. Off unless set.
    ///
    /// Writes `up.bin`, `down.bin`, `inject.bin` and `blocked.bin`: raw octets exactly as they crossed the
    /// socket, so a later, better decoder can re-read them.
    #[arg(long, env = "HELIOBRIDGE_RECORD_DIR", value_parser = path_allowing_empty)]
    pub record_dir: Option<PathBuf>,

    /// Serve the control API on this Unix socket. Off unless set.
    ///
    /// HTTP, so it is reachable with `curl --unix-socket`. Routes are scoped by device, e.g.
    /// `/devices/<serial>/settings/slot1_output_power`. Created mode 0600; there is no network listener.
    #[arg(long, env = "HELIOBRIDGE_CONTROL_SOCKET", value_parser = path_allowing_empty)]
    pub control_socket: Option<PathBuf>,

    /// Cap per recording stream, in bytes.
    ///
    /// On reaching it the file rotates once to `.1`, keeping the most recent window rather than stopping
    /// at the least useful moment. Telemetry alone is roughly 10 MB per day.
    #[arg(long, env = "HELIOBRIDGE_RECORD_MAX_BYTES", default_value_t = record::DEFAULT_MAX_BYTES)]
    pub record_max_bytes: u64,

    /// How many schedule slots to read back and expose, 1–9.
    ///
    /// The device has nine, each five registers. Nine would be 45 entities for hardware that in practice
    /// runs a single all-day slot, so the default keeps things readable while the capability stays
    /// available.
    #[arg(long, env = "HELIOBRIDGE_SLOTS", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=9))]
    pub slots: u16,

    /// Broker to publish Home Assistant entities to.
    ///
    /// `mqtt://host[:port]` for a plain connection, `mqtts://host[:port]` for TLS. Home Assistant
    /// publishing is off entirely while this is unset.
    #[arg(long, env = "HELIOBRIDGE_MQTT_URL")]
    pub mqtt_url: Option<String>,

    /// Username for the broker, if it wants one.
    #[arg(long, env = "HELIOBRIDGE_MQTT_USER")]
    pub mqtt_user: Option<String>,

    /// Password for the broker.
    ///
    /// Prefer `--mqtt-pass-file`: an environment variable is readable through `/proc`, appears in a
    /// systemd unit, and is inherited by anything this process spawns.
    #[arg(long, env = "HELIOBRIDGE_MQTT_PASS")]
    pub mqtt_pass: Option<String>,

    /// File holding the broker password, read at startup.
    ///
    /// A relative path is resolved inside `$CREDENTIALS_DIRECTORY` when systemd provides one, so
    /// `LoadCredential=mqtt-pass:/etc/heliobridge/mqtt.pass` pairs with `--mqtt-pass-file mqtt-pass`.
    /// Trailing newlines are stripped. Takes precedence over `--mqtt-pass`.
    #[arg(long, env = "HELIOBRIDGE_MQTT_PASS_FILE", value_parser = path_allowing_empty)]
    pub mqtt_pass_file: Option<PathBuf>,

    /// PEM certificate chain to present to the broker, for a broker that authenticates by certificate.
    ///
    /// Only meaningful with `mqtts://`. Requires `--mqtt-client-key`.
    #[arg(long, env = "HELIOBRIDGE_MQTT_CLIENT_CERT", value_parser = path_allowing_empty)]
    pub mqtt_client_cert: Option<PathBuf>,

    /// PEM private key matching `--mqtt-client-cert`.
    #[arg(long, env = "HELIOBRIDGE_MQTT_CLIENT_KEY", value_parser = path_allowing_empty)]
    pub mqtt_client_key: Option<PathBuf>,

    /// Root of this program's own topics on the broker.
    #[arg(long, env = "HELIOBRIDGE_MQTT_BASE", default_value = DEFAULT_MQTT_BASE)]
    pub mqtt_base: String,

    /// Root Home Assistant watches for discovery messages.
    ///
    /// Change it only to match a Home Assistant that was configured with a non-default prefix.
    #[arg(long, env = "HELIOBRIDGE_MQTT_DISCOVERY_PREFIX", default_value = DEFAULT_DISCOVERY_PREFIX)]
    pub mqtt_discovery_prefix: String,

    /// What distinguishes this bridge from another on the same broker. Defaults to the host name.
    ///
    /// It appears in one topic only — this program's own availability — since everything else is keyed by
    /// device serial. Two bridges sharing it would mark each other's entities unavailable on shutdown.
    #[arg(long, env = "HELIOBRIDGE_MQTT_INSTANCE")]
    pub mqtt_instance: Option<String>,

    /// Offer settings as Home Assistant controls rather than as read-only sensors.
    ///
    /// `false` publishes every setting as a plain sensor and accepts no commands, which is what to run
    /// alongside another controller so two writers are not fighting over the same registers.
    #[arg(long, env = "HELIOBRIDGE_ALLOW_WRITES", default_value_t = true, action = clap::ArgAction::Set)]
    pub allow_writes: bool,

    /// Allow `power_plus` to be written from Home Assistant.
    ///
    /// `false` publishes it as a read-only sensor and refuses any command naming it, in either direction. It
    /// stays visible: whether it is on is worth seeing even where this bridge may not set it, since the
    /// vendor app still can.
    ///
    /// Like `--allow-writes`, this governs the Home Assistant surface. The control socket is the owner's own
    /// interface — mode 0600, and the one place that already serves the serial and the password — and it
    /// writes every allowlisted setting whatever this says.
    #[arg(long, env = "HELIOBRIDGE_ALLOW_POWER_PLUS", default_value_t = true, action = clap::ArgAction::Set)]
    pub allow_power_plus: bool,

    /// Seconds without a telemetry frame before the device is reported absent.
    ///
    /// Telemetry arrives every five seconds, so the default is six missed cycles. It exists because a
    /// half-open connection looks alive: the device's own MQTT keepalive is 420 s, and waiting for that
    /// would leave stale readings on a dashboard for seven minutes.
    #[arg(long, env = "HELIOBRIDGE_OFFLINE_AFTER", default_value_t = publisher::OFFLINE_AFTER.as_secs())]
    pub offline_after: u64,

    /// Relay device traffic to the Growatt cloud, so the vendor app keeps working.
    ///
    /// How much authority the cloud then keeps is a separate decision — see `--relay-mode`, which also
    /// decides who owns the device's clock, since the vendor server sets it with a configuration write.
    #[arg(long, env = "HELIOBRIDGE_CLOUD_RELAY", default_value_t = false)]
    pub cloud_relay: bool,

    /// Cloud endpoint to relay to, as `host:port`.
    ///
    /// The host is also the TLS server name, so it must be a name rather than an address.
    #[arg(long, env = "HELIOBRIDGE_CLOUD_HOST", default_value = cloud::DEFAULT_HOST)]
    pub cloud_host: String,

    /// Cloud port.
    #[arg(long, env = "HELIOBRIDGE_CLOUD_PORT", default_value_t = cloud::DEFAULT_PORT)]
    pub cloud_port: u16,

    /// How much authority the vendor cloud keeps while relaying.
    ///
    /// In every mode the vendor app keeps **displaying** correctly; what differs is what it may change.
    ///
    /// - `full` — the app works as if this program were absent, including datalogger configuration. The
    ///   cloud then also owns the clock, and could retarget the device's broker away from here.
    /// - `controls` — the app still changes slots, output power, charge limits and the switches, but not the
    ///   broker endpoint, DNS, timezone or clock, and nothing unrecognised. The vendor server was never
    ///   observed sending anything outside the permitted set, so this costs no observed functionality;
    ///   refusing the unrecognised is also the only available defence against a message nobody can classify.
    /// - `observer` — the cloud sees everything and changes nothing. The right choice once settings are
    ///   driven locally, since a second writer is then only a way for two pictures to disagree.
    ///
    /// Nothing the device sends is ever withheld, in any mode: a report cannot change the device's behaviour,
    /// and the vendor app's display is fed from the cloud's store, so withholding one only makes the app
    /// wrong — and an app writing whole register ranges from a wrong picture reverts settings.
    ///
    /// Worth remembering in every mode: "the cloud" is anyone who can reach the vendor broker knowing this
    /// serial.
    #[arg(long, env = "HELIOBRIDGE_RELAY_MODE", default_value = "controls")]
    pub relay_mode: Mode,

    /// Which answers to earlier commands are forwarded to the cloud while relaying.
    ///
    /// `cloud-only`, the default, forwards only answers to commands the cloud itself issued. Every local write
    /// produces an acknowledgement and a read-back, and every reconnect re-reads each exposed setting, so a
    /// controller driving the device turns those into a steady stream of frames the cloud never asked for.
    ///
    /// Forwarding them was measured to achieve nothing: the vendor app's settings view is updated by the
    /// periodic snapshot alone — neither acknowledgements nor read responses move it — so the app trails the
    /// device by up to an hour either way. Unrequested traffic with no upside is worth avoiding against a
    /// vendor whose APIs are documented as rate-limiting and IP-banning.
    ///
    /// Reports are never withheld in either setting: telemetry, identity and the settings snapshot always pass.
    #[arg(long, env = "HELIOBRIDGE_RELAY_ANSWERS", default_value = "cloud-only")]
    pub relay_answers: Answers,

    /// Tracing filter, per subsystem. Falls back to `RUST_LOG`.
    #[arg(long, env = "HELIOBRIDGE_LOG", default_value = DEFAULT_LOG)]
    pub log: String,

    /// Log format.
    #[arg(long, env = "HELIOBRIDGE_LOG_FORMAT", default_value = "text")]
    pub log_format: LogFormat,
}

/// Accept a path written as an empty string, leaving [`Config::ignoring_empty`] to treat it as absent.
///
/// clap's own path parser refuses an empty value, which is what made a cleared variable mean "error" for a
/// setting that names a path and "off" for one that does not.
#[expect(
    clippy::unnecessary_wraps,
    reason = "clap requires a value parser to return a Result; this one accepts everything a path can be, \
              which is the whole point of replacing the one that refuses an empty value."
)]
fn path_allowing_empty(value: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(value))
}

/// A setting given as an empty string, treated as one that was not given.
fn given(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

/// The same, for a setting that names a path.
fn given_path(value: Option<PathBuf>) -> Option<PathBuf> {
    value.filter(|path| !path.as_os_str().is_empty())
}

/// A defaulted setting, falling back to its default when cleared.
fn given_or(value: String, default: &str) -> String {
    if value.is_empty() { default.to_owned() } else { value }
}

/// How log records are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable single lines.
    Text,
    /// One JSON object per record, so typed fields survive to a log aggregator.
    Json,
}

impl Config {
    /// Parse from the environment and the command line.
    pub fn from_env() -> Self {
        Self::parse().ignoring_empty()
    }

    /// Treat a setting cleared to an empty string as one that was never given.
    ///
    /// A variable that is always *set* and sometimes empty is how the container world configures things:
    /// Compose materialises every key in the `.env` file, so `HELIOBRIDGE_MQTT_URL=` is the only way to turn
    /// publishing off short of maintaining two files. Absence is not available there.
    ///
    /// So empty means the same as absent: an optional setting is off, and one with a default falls back to
    /// it. The two allowlists are untouched, because empty is already their meaningful value — it is how
    /// they say "admit everything".
    #[must_use]
    fn ignoring_empty(mut self) -> Self {
        self.tls_cert = given_path(self.tls_cert);
        self.tls_key = given_path(self.tls_key);
        self.record_dir = given_path(self.record_dir);
        self.control_socket = given_path(self.control_socket);
        self.mqtt_url = given(self.mqtt_url);
        self.mqtt_user = given(self.mqtt_user);
        self.mqtt_pass = given(self.mqtt_pass);
        self.mqtt_pass_file = given_path(self.mqtt_pass_file);
        self.mqtt_client_cert = given_path(self.mqtt_client_cert);
        self.mqtt_client_key = given_path(self.mqtt_client_key);
        self.mqtt_instance = given(self.mqtt_instance);

        self.state_dir = given_path(Some(self.state_dir)).unwrap_or_else(default_state_dir);
        self.mqtt_base = given_or(self.mqtt_base, DEFAULT_MQTT_BASE);
        self.mqtt_discovery_prefix = given_or(self.mqtt_discovery_prefix, DEFAULT_DISCOVERY_PREFIX);
        self.cloud_host = given_or(self.cloud_host, cloud::DEFAULT_HOST);
        self.log = given_or(self.log, DEFAULT_LOG);
        self
    }

    /// Whether this server should push its wall-clock time to the device.
    ///
    /// Exactly one party may own the clock. The vendor server sets it with a configuration write, so the
    /// question is not "are we relaying" but "is that write getting through": if the mode refuses it, nobody
    /// is setting the clock unless we do. In the default mode that is always, relay or not.
    pub const fn should_push_time(&self) -> bool {
        !(self.cloud_relay && self.relay_mode.cloud_may_write_config())
    }

    /// What the relay carries in each direction.
    pub const fn policy(&self) -> Policy {
        Policy {
            mode: self.relay_mode,
            answers: self.relay_answers,
        }
    }

    /// The cloud endpoint to relay to, or `None` when relaying is off.
    pub fn cloud(&self) -> Option<CloudConfig> {
        self.cloud_relay.then(|| CloudConfig {
            host: self.cloud_host.clone(),
            port: self.cloud_port,
        })
    }

    /// The broker password, from a file if one was named and from the environment otherwise.
    ///
    /// # Errors
    ///
    /// A message naming the file if it cannot be read. Startup fails rather than continuing without a
    /// password, which would present as an authentication failure against the broker and send the reader
    /// looking at the broker's configuration instead of at a missing file.
    pub fn mqtt_password(&self) -> Result<Option<String>, String> {
        let Some(path) = self.mqtt_pass_file.as_ref() else {
            return Ok(self.mqtt_pass.clone());
        };

        // systemd puts credentials in a directory it names, with the unit referring to them by bare name.
        // An absolute path is used as given, so this only ever adds a way to spell it.
        let path = match std::env::var_os("CREDENTIALS_DIRECTORY") {
            Some(dir) if path.is_relative() => PathBuf::from(dir).join(path),
            _ => path.clone(),
        };

        let secret = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read the broker password from {}: {error}", path.display()))?;
        // A file written with an editor ends in a newline, which is not part of the password.
        Ok(Some(secret.trim_end_matches(['\r', '\n']).to_owned()))
    }

    /// The client certificate and key to present to the broker, if both were named.
    ///
    /// # Errors
    ///
    /// A message if only one of the pair was given: a certificate without its key cannot authenticate
    /// anything, and silently ignoring half of it would look like the broker rejecting valid credentials.
    pub fn mqtt_client_identity(&self) -> Result<Option<(PathBuf, PathBuf)>, String> {
        match (self.mqtt_client_cert.as_ref(), self.mqtt_client_key.as_ref()) {
            (Some(cert), Some(key)) => Ok(Some((cert.clone(), key.clone()))),
            (None, None) => Ok(None),
            (Some(_), None) => Err("--mqtt-client-cert needs --mqtt-client-key".to_owned()),
            (None, Some(_)) => Err("--mqtt-client-key needs --mqtt-client-cert".to_owned()),
        }
    }

    /// Which addresses the device may connect from.
    ///
    /// # Errors
    ///
    /// A message naming the entry that could not be read. Startup fails rather than continuing with a list
    /// that admits less than the operator wrote — locking the device out is worse than not filtering.
    pub fn peers(&self) -> Result<Peers, String> {
        Peers::parse(&self.allow_from).map_err(|error| format!("--allow-from: {error}"))
    }

    /// Which device serials to serve.
    pub fn devices(&self) -> Devices {
        Devices::parse(&self.allow_devices)
    }

    /// What everything is called on the broker.
    pub fn topics(&self) -> Topics {
        Topics {
            base: self.mqtt_base.clone(),
            discovery_prefix: self.mqtt_discovery_prefix.clone(),
            instance: self.mqtt_instance.clone().unwrap_or_else(topics::default_instance),
        }
    }

    /// What gets published, as against where.
    pub const fn publishing(&self) -> PublisherOptions {
        PublisherOptions {
            slots: self.slots,
            permitted: Permitted {
                writes: self.allow_writes,
                power_plus: self.allow_power_plus,
            },
            offline_after: Duration::from_secs(self.offline_after),
        }
    }

    /// Where to record frames, or `None` when recording is off.
    pub fn recording(&self) -> Option<RecorderConfig> {
        self.record_dir.as_ref().map(|dir| RecorderConfig {
            dir: dir.clone(),
            max_bytes: self.record_max_bytes,
        })
    }

    /// Whether both halves of a supplied certificate are present.
    ///
    /// Supplying only one is a configuration mistake worth naming rather than silently falling back to
    /// a generated certificate, which would look like the supplied one being ignored.
    ///
    /// # Errors
    ///
    /// A message naming which half is missing, suitable for printing as-is.
    pub const fn tls_pair(&self) -> Result<Option<(&PathBuf, &PathBuf)>, &'static str> {
        match (self.tls_cert.as_ref(), self.tls_key.as_ref()) {
            (Some(cert), Some(key)) => Ok(Some((cert, key))),
            (None, None) => Ok(None),
            (Some(_), None) => Err("HELIOBRIDGE_TLS_CERT is set but HELIOBRIDGE_TLS_KEY is not"),
            (None, Some(_)) => Err("HELIOBRIDGE_TLS_KEY is set but HELIOBRIDGE_TLS_CERT is not"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Answers, Config, DEFAULT_DISCOVERY_PREFIX, DEFAULT_LISTEN, DEFAULT_LOG, DEFAULT_MQTT_BASE, LogFormat, Mode,
        default_state_dir,
    };
    use clap::Parser as _;

    fn parse(flags: &[&str]) -> Config {
        let mut command_line = vec!["heliobridge"];
        command_line.extend_from_slice(flags);
        Config::try_parse_from(command_line).expect("parse")
    }

    #[test]
    fn defaults_listen_on_the_device_port() {
        let config = parse(&[]);
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(config.listen.port(), 7006);
        assert_eq!(config.log_format, LogFormat::Text);
        assert!(config.record_dir.is_none(), "recording is off by default");
        assert!(config.tls_cert.is_none());
    }

    #[test]
    fn publishing_to_home_assistant_is_off_until_a_broker_is_named() {
        // A supported mode, not a degraded one: the device server and the control API are useful on their
        // own, and that is how the protocol work was done. Nothing may dial a broker that was never named.
        let config = parse(&[]);
        assert!(config.mqtt_url.is_none());
        assert!(config.mqtt_user.is_none() && config.mqtt_pass.is_none());

        // The defaults for the rest still have to be sane, since they are read as soon as one is named.
        let publishing = config.publishing();
        assert_eq!(publishing.slots, 1);
        assert!(publishing.permitted.writes);
        assert!(publishing.permitted.power_plus);
        assert_eq!(publishing.offline_after, super::publisher::OFFLINE_AFTER);
        assert_eq!(config.topics().base, "heliobridge");
        assert_eq!(config.topics().discovery_prefix, "homeassistant");
        assert!(!config.topics().instance.is_empty());
    }

    #[test]
    fn clearing_a_setting_turns_it_off_exactly_as_leaving_it_out_would() {
        // Compose materialises every key in the `.env` file, so a variable is always set and an empty value
        // is the only way to turn something off. Absence is not available there.
        let cleared = parse(&[
            "--mqtt-url",
            "",
            "--mqtt-user",
            "",
            "--mqtt-pass",
            "",
            "--mqtt-pass-file",
            "",
            "--mqtt-instance",
            "",
            "--record-dir",
            "",
            "--control-socket",
            "",
            "--tls-cert",
            "",
            "--tls-key",
            "",
            "--mqtt-client-cert",
            "",
            "--mqtt-client-key",
            "",
        ])
        .ignoring_empty();

        assert!(cleared.mqtt_url.is_none(), "publishing is off");
        assert!(cleared.record_dir.is_none(), "recording is off");
        assert!(cleared.control_socket.is_none(), "the control API is off");
        assert!(cleared.tls_pair().expect("neither half is set").is_none());
        assert!(cleared.mqtt_client_identity().expect("neither half is set").is_none());
        assert_eq!(cleared.mqtt_password().expect("nothing to read"), None);
        assert!(cleared.mqtt_user.is_none() && cleared.mqtt_instance.is_none());
    }

    #[test]
    fn clearing_a_defaulted_setting_falls_back_to_its_default() {
        let cleared = parse(&[
            "--mqtt-base",
            "",
            "--mqtt-discovery-prefix",
            "",
            "--state-dir",
            "",
            "--cloud-host",
            "",
            "--log",
            "",
        ])
        .ignoring_empty();

        assert_eq!(cleared.mqtt_base, DEFAULT_MQTT_BASE);
        assert_eq!(cleared.mqtt_discovery_prefix, DEFAULT_DISCOVERY_PREFIX);
        assert_eq!(cleared.state_dir, default_state_dir());
        assert_eq!(cleared.log, DEFAULT_LOG);
        assert!(!cleared.cloud_host.is_empty());
        // An empty base would make every published topic start with a slash.
        assert_eq!(cleared.topics().base, DEFAULT_MQTT_BASE);
    }

    #[test]
    fn an_allowlist_keeps_its_empty_value_because_that_is_what_it_means() {
        // The exception. Empty is already the documented value here: admit everything.
        let config = parse(&["--allow-from", "", "--allow-devices", ""]).ignoring_empty();
        assert!(config.peers().expect("empty parses").is_open());
        assert!(config.devices().is_open());
    }

    #[test]
    fn an_incomplete_tls_pair_is_an_error_not_a_silent_fallback() {
        let config = parse(&["--tls-cert", "/tmp/a.crt"]);
        assert!(config.tls_pair().is_err());

        let config = parse(&["--tls-key", "/tmp/a.key"]);
        assert!(config.tls_pair().is_err());

        let config = parse(&["--tls-cert", "/tmp/a.crt", "--tls-key", "/tmp/a.key"]);
        assert!(config.tls_pair().expect("valid").is_some());

        assert!(parse(&[]).tls_pair().expect("valid").is_none());
    }

    #[test]
    fn the_listener_address_is_validated_at_parse_time() {
        assert!(Config::try_parse_from(["heliobridge", "--listen", "not-an-address"]).is_err());
        let config = parse(&["--listen", "127.0.0.1:17006"]);
        assert_eq!(config.listen.port(), 17006);
    }

    #[test]
    fn the_clock_authority_follows_the_policy_not_the_relay() {
        // Not relaying: nobody else could set it.
        assert!(parse(&[]).should_push_time());
        // Relaying in the default mode, which refuses the cloud's configuration write: if we did not push,
        // the device's clock would go unset.
        assert!(parse(&["--cloud-relay"]).should_push_time());
        assert!(parse(&["--cloud-relay", "--relay-mode", "observer"]).should_push_time());
        // Full mode: the cloud's write gets through, so pushing as well would set the clock twice per
        // connect, to two slightly different values.
        assert!(!parse(&["--cloud-relay", "--relay-mode", "full"]).should_push_time());
    }

    #[test]
    fn the_default_keeps_the_app_working_minus_configuration() {
        let config = parse(&[]);
        assert_eq!(config.relay_mode, Mode::Controls);
        assert!(
            !config.relay_mode.cloud_may_write_config(),
            "no datalogger configuration"
        );
        assert_eq!(
            config.relay_answers,
            Answers::CloudOnly,
            "answers to our own commands are not forwarded; the cloud ignores them anyway"
        );
    }

    #[test]
    fn log_format_accepts_only_known_values() {
        assert_eq!(parse(&["--log-format", "json"]).log_format, LogFormat::Json);
        assert!(Config::try_parse_from(["heliobridge", "--log-format", "yaml"]).is_err());
    }
}
