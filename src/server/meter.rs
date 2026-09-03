//! A simulated energy meter, answered on the device's own port.
//!
//! The datalogger does not wait to be told a load figure — it **fetches** one, as an ordinary HTTP client,
//! from an address it has been given. So supplying a reading means answering `GET /status` the way a Shelly
//! generation 1 energy meter does, and pointing the device at us.
//!
//! # Why this shares the MQTT port
//!
//! The device may reach exactly one thing on this network: TCP 7006 on this host. Its own firewall permits
//! that and nothing else, so a meter served on port 80 would never be fetched. Plain HTTP and TLS are
//! distinguishable on their first octet, though — a request begins `G` of `GET`, a TLS record begins
//! `0x16` — so one listener can carry both and the device needs no new path opened for it.
//!
//! The port is then encoded in the address the device is given, which is possible because the field it
//! parses is an opaque host string rather than a validated IP, and the URL layer behind it splits a colon
//! itself. See `RF-FINDINGS.md` and the specification's Appendix C.
//!
//! # Which meter, and why this one
//!
//! Generation 1 is the cheapest of the five shapes the firmware knows: a flat `total_power` and one
//! `emeters` entry, against device type 1. Generation 2 would need a nested `em:0` object with per-phase
//! fields, and the `HomeWizard` and `EcoTracker` variants are no simpler. The field names below are the ones
//! the firmware carries as string constants, not a guess at the vendor's API.
//!
//! # What it is not
//!
//! Not a general web server. It answers one path, ignores headers, keeps no connection alive, and reads a
//! bounded amount before replying — a fetch from an embedded HTTP client is a single small request, and
//! anything else here is either a mistake or somebody scanning.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How much of a request to read before answering.
///
/// A request line and a few headers. The reply does not depend on any of it, so this exists only to drain
/// what the client sent rather than to parse it.
const REQUEST_LIMIT: usize = 1024;

/// The simulated meter, and the readings it reports.
///
/// A process-wide value rather than something threaded through the session options: there is one simulated
/// meter, every connection answers from the same figures, and the control API changes them while
/// connections are in flight. Atomics rather than a lock because each field is read and written whole.
#[derive(Debug)]
pub struct Meter {
    /// Active power in watts. Signed: import is positive, export negative, as a real meter reports.
    watts: AtomicI64,
    /// Whether to answer at all. Off means a non-TLS connection is dropped as before.
    enabled: AtomicBool,
    /// How many requests have been answered, which is the whole point of the experiment.
    served: AtomicU64,
}

/// The one simulated meter.
pub static METER: Meter = Meter::new();

impl Meter {
    /// Off, reporting nothing.
    const fn new() -> Self {
        Self {
            watts: AtomicI64::new(0),
            enabled: AtomicBool::new(false),
            served: AtomicU64::new(0),
        }
    }

    /// Begin answering requests, reporting `watts`.
    pub fn enable(&self, watts: i64) {
        self.watts.store(watts, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Relaxed);
    }

    /// Stop answering. A non-TLS connection is then dropped, as it was before this existed.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }

    /// Whether requests are being answered.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Report a different figure from now on.
    pub fn set_watts(&self, watts: i64) {
        self.watts.store(watts, Ordering::Relaxed);
    }

    /// What is being reported.
    pub fn watts(&self) -> i64 {
        self.watts.load(Ordering::Relaxed)
    }

    /// How many requests have been answered since start.
    pub fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// The body a Shelly generation 1 energy meter returns from `/status`.
    ///
    /// Every field here is one the datalogger carries as a string constant. `total_power` is the figure it
    /// is after; the `emeters` entry exists because the firmware looks for that array too, and a meter that
    /// reported a total with no phases would be a shape no real device produces.
    ///
    /// Voltage and current are plausible rather than measured: 230 V, and a current consistent with the
    /// power at unity factor. `is_valid` matters — a real meter clears it when a channel is unreadable, and
    /// a client may well skip an invalid one.
    fn body(&self) -> String {
        let watts = self.watts();
        // Deliberately integer arithmetic in milliamps: a current rendered from floating point invites a
        // reading like 1.0999999, and nothing here needs sub-milliamp resolution.
        let milliamps = watts.saturating_mul(1000).saturating_div(230);
        let amps = milliamps.saturating_div(1000);
        let millis = milliamps.checked_rem(1000).unwrap_or_default().abs();
        format!(
            concat!(
                "{{\"total_power\":{watts}.00,",
                "\"emeters\":[",
                "{{\"power\":{watts}.00,\"voltage\":230.00,\"current\":{amps}.{millis:03},",
                "\"total\":0.0,\"total_returned\":0.0,\"is_valid\":true}},",
                "{{\"power\":0.00,\"voltage\":230.00,\"current\":0.000,",
                "\"total\":0.0,\"total_returned\":0.0,\"is_valid\":true}},",
                "{{\"power\":0.00,\"voltage\":230.00,\"current\":0.000,",
                "\"total\":0.0,\"total_returned\":0.0,\"is_valid\":true}}",
                "]}}"
            ),
            watts = watts,
            amps = amps,
            millis = millis,
        )
    }

    /// Answer one connection, then close it.
    ///
    /// Errors are logged rather than returned: this is a side experiment on a shared port, and a client
    /// that hangs up mid-request must not disturb anything else.
    pub async fn serve(&self, mut stream: TcpStream, peer: std::net::SocketAddr) {
        let mut request = vec![0_u8; REQUEST_LIMIT];
        let read = match stream.read(&mut request).await {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%peer, %error, "meter request could not be read");
                return;
            }
        };
        let head = String::from_utf8_lossy(request.get(..read).unwrap_or_default()).to_string();
        let line = head.lines().next().unwrap_or_default().to_owned();

        // `/status` is the only path a generation 1 meter serves, and the only one the datalogger asks for.
        // Anything else is answered honestly rather than with the meter body, so a scan does not come away
        // thinking this is a Shelly.
        let wants_status = line.starts_with("GET /status");
        let response = if wants_status {
            let body = self.body();
            self.served.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                %peer,
                watts = self.watts(),
                served = self.served(),
                "answered a meter poll"
            );
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        } else {
            tracing::info!(%peer, request = %line, "declined a request that is not a meter poll");
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        };

        if let Err(error) = stream.write_all(response.as_bytes()).await {
            tracing::warn!(%peer, %error, "meter reply could not be sent");
            return;
        }
        // Flush before dropping: the reply is small enough to sit in the buffer, and a client that reads
        // until close would otherwise see nothing.
        if let Err(error) = stream.flush().await {
            tracing::warn!(%peer, %error, "meter reply could not be flushed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{METER, Meter};

    #[test]
    fn the_body_carries_every_field_the_firmware_looks_for() {
        // These are the string constants read out of the datalogger image. A body missing one of them may
        // parse and still leave the device with no figure, which would look like the device ignoring us.
        let meter = Meter::new();
        meter.enable(250);
        let body = meter.body();
        for field in [
            "total_power",
            "emeters",
            "power",
            "voltage",
            "current",
            "total",
            "total_returned",
            "is_valid",
        ] {
            assert!(body.contains(field), "{field} missing from the meter body");
        }
    }

    #[test]
    fn the_reported_power_appears_as_the_total_and_the_first_phase() {
        let meter = Meter::new();
        meter.enable(415);
        let body = meter.body();
        assert!(body.contains("\"total_power\":415.00"), "{body}");
        assert!(body.contains("\"power\":415.00"), "{body}");
        // Three phases, because a 3EM reports three and the second and third are idle here.
        assert_eq!(body.matches("is_valid").count(), 3, "{body}");
    }

    #[test]
    fn export_is_reported_as_a_negative_figure() {
        // A real meter signs the direction, and the whole point of a load reading is which way it flows.
        let meter = Meter::new();
        meter.enable(-400);
        let body = meter.body();
        assert!(body.contains("\"total_power\":-400.00"), "{body}");
        assert!(body.contains("\"current\":-1.739"), "{body}");
    }

    #[test]
    fn current_follows_the_power_at_mains_voltage() {
        let meter = Meter::new();
        meter.enable(230);
        assert!(meter.body().contains("\"current\":1.000"), "{}", meter.body());
    }

    #[test]
    fn it_is_off_until_asked() {
        // The listener drops a non-TLS connection unless this is on, so the default decides whether a
        // stray HTTP request to the device's port gets an answer at all.
        assert!(!METER.enabled());
    }
}
