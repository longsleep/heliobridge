//! Configuration, entirely from the environment.
//!
//! `clap` derive gives the environment variables, `--help`, validation and defaults from one
//! definition. Every variable is prefixed `HELIOBRIDGE_`.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::growatt::cloud::{self, CloudConfig};
use crate::record::{self, RecorderConfig};

/// Default device-facing listener address.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:7006";

/// Default state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/heliobridge";

/// A local MQTT bridge for the Growatt Nexa 2000.
#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// Device-facing TLS listener.
    ///
    /// Port 7006 is where the device connects; changing it only makes sense alongside a destination
    /// NAT rule that rewrites the port.
    #[arg(long, env = "HELIOBRIDGE_LISTEN", default_value = DEFAULT_LISTEN)]
    pub listen: SocketAddr,

    /// PEM certificate to present to the device. Generated on first run if unset.
    #[arg(long, env = "HELIOBRIDGE_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key matching `--tls-cert`.
    #[arg(long, env = "HELIOBRIDGE_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Where to keep the generated certificate and cached state.
    #[arg(long, env = "HELIOBRIDGE_STATE_DIR", default_value = DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,

    /// Record every frame here for later analysis. Off unless set.
    ///
    /// Writes `up.bin`, `down.bin` and `inject.bin`: raw octets exactly as they crossed the socket, so a
    /// later, better decoder can re-read them.
    #[arg(long, env = "HELIOBRIDGE_RECORD_DIR")]
    pub record_dir: Option<PathBuf>,

    /// Serve the control API on this Unix socket. Off unless set.
    ///
    /// HTTP, so it is reachable with `curl --unix-socket`. Routes are scoped by device, e.g.
    /// `/devices/<serial>/settings/slot1_output_power`. Created mode 0600; there is no network listener.
    #[arg(long, env = "HELIOBRIDGE_CONTROL_SOCKET")]
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

    /// Relay device traffic to the Growatt cloud, so the vendor app keeps working.
    ///
    /// Also decides who sets the device's clock. The server time push is sent unless this is on: the
    /// cloud sends its own, and two servers setting one clock is one too many — the device would be set
    /// twice per connect, to values differing by whatever skew exists between them.
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

    /// Tracing filter, per subsystem. Falls back to `RUST_LOG`.
    #[arg(long, env = "HELIOBRIDGE_LOG", default_value = "info")]
    pub log: String,

    /// Log format.
    #[arg(long, env = "HELIOBRIDGE_LOG_FORMAT", default_value = "text")]
    pub log_format: LogFormat,
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
        Self::parse()
    }

    /// Whether this server should push its wall-clock time to the device.
    ///
    /// Always, unless relaying — in which case the cloud does it and we would be a second authority on
    /// the same clock.
    pub const fn should_push_time(&self) -> bool {
        !self.cloud_relay
    }

    /// The cloud endpoint to relay to, or `None` when relaying is off.
    pub fn cloud(&self) -> Option<CloudConfig> {
        self.cloud_relay.then(|| CloudConfig {
            host: self.cloud_host.clone(),
            port: self.cloud_port,
        })
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
    use super::{Config, DEFAULT_LISTEN, LogFormat};
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
    fn log_format_accepts_only_known_values() {
        assert_eq!(parse(&["--log-format", "json"]).log_format, LogFormat::Json);
        assert!(Config::try_parse_from(["heliobridge", "--log-format", "yaml"]).is_err());
    }
}
