//! The device-facing TLS listener.
//!
//! Accepts connections, wraps them in TLS and hands each to a [`crate::server::Session`]. One device is
//! expected, but connections are handled concurrently anyway: the device reconnects aggressively, and a
//! stale socket must not be able to lock out the live one.
//!
//! # Failures are per-connection
//!
//! Nothing a session does takes the listener down. The device has nowhere else to publish, so a failed
//! handshake, a dropped connection or a malformed packet are logged and forgotten — availability outranks
//! strictness here. Even the cloud relay disappearing only ends its session: the relay is built per
//! session, so the device's reconnect a couple of seconds later brings up a fresh one.

use core::future::Future;
use core::pin::pin;
use std::sync::Arc;

use rustls::ServerConfig;
use snafu::{ResultExt, Snafu};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::control::Registry;
use crate::growatt::cloud::CloudRelay;
use crate::growatt::policy::Policy;
use crate::record::Recorder;
use crate::server::access::{Devices, Peers};
use crate::server::probe;
use crate::server::session::Session;

/// Why the listener stopped.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ListenerError {
    /// The listening socket could not be bound.
    #[snafu(display("could not bind {address}"))]
    Bind {
        /// Address attempted.
        address: std::net::SocketAddr,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// How each session should behave.
///
/// Not comparable: it carries a [`Recorder`] handle, and two handles to the same recorder are the same
/// recorder in every sense that matters, which is not a thing equality can usefully express.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Whether to push the server's wall-clock time after the device connects.
    ///
    /// Off only when the cloud's own configuration write is being relayed through, since two parties
    /// setting one clock would set it twice per connect, to values differing by whatever skew exists
    /// between them.
    pub time_push: bool,

    /// Relay traffic to the vendor cloud, so the phone app keeps working.
    pub cloud: Option<CloudRelay>,

    /// What the relay carries in each direction. Ignored unless `cloud` is set.
    pub policy: Policy,

    /// Record every frame, in both directions plus the ones this program originates.
    pub recorder: Option<Recorder>,

    /// How many schedule slots to read back at startup.
    pub slots: u16,

    /// Where sessions announce themselves so the control API can address them by device.
    pub registry: Option<Registry>,

    /// Which device serials may be served. Empty admits any.
    pub devices: Devices,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            time_push: true,
            cloud: None,
            policy: Policy::default(),
            recorder: None,
            slots: 1,
            registry: None,
            devices: Devices::default(),
        }
    }
}

/// Serve until the shutdown signal fires.
///
/// # Errors
///
/// [`ListenerError::Bind`] if the address is unavailable. Per-connection failures are logged and do not
/// stop the listener.
pub async fn serve(
    address: std::net::SocketAddr,
    tls: Arc<ServerConfig>,
    options: SessionOptions,
    peers: Peers,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ListenerError> {
    let listener = TcpListener::bind(address).await.context(BindSnafu { address })?;
    let acceptor = TlsAcceptor::from(tls);

    tracing::info!(
        %address,
        time_push = options.time_push,
        cloud_relay = options.cloud.is_some(),
        recording = options.recorder.is_some(),
        mode = ?options.policy.mode,
        answers = ?options.policy.answers,
        accept_from = %peers,
        serve_devices = %options.devices,
        "listening for the device"
    );

    let mut shutdown = pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!("shutting down the listener");
                return Ok(());
            }

            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        // Transient accept failures — a peer that vanished mid-handshake, a momentary
                        // descriptor shortage — must not take the listener down.
                        tracing::warn!(%error, "accept failed");
                        continue;
                    }
                };
                // A liveness probe, before everything else. Loopback only, so nothing here is reachable
                // from the network the device is on; and ahead of the allowlist, or a deployment that
                // admits only the device's address would refuse its own healthcheck. A connection that
                // is not a probe comes back with every octet unread.
                let mut stream = stream;
                if peer.ip().is_loopback() && probe::Probe::new(&mut stream).serve().await == probe::Outcome::Answered {
                    tracing::debug!(%peer, "answered a liveness probe");
                    continue;
                }

                // Before the handshake, so an unwanted peer costs a socket and a log line rather than a
                // certificate exchange — and so nothing it sends is ever parsed.
                if !peers.admits(peer.ip()) {
                    tracing::warn!(%peer, allowed = %peers, "refusing a connection from an address that is not allowed");
                    drop(stream);
                    continue;
                }

                let acceptor = acceptor.clone();
                let options = options.clone();
                tokio::spawn(async move {
                    handle(stream, peer, acceptor, options).await;
                });
            }
        }
    }
}

/// Complete the TLS handshake and run one session.
#[tracing::instrument(skip(stream, acceptor, options), fields(%peer))]
async fn handle(stream: TcpStream, peer: std::net::SocketAddr, acceptor: TlsAcceptor, options: SessionOptions) {
    // Nagle off: the device waits for small acknowledgements, and delaying a PUBACK to coalesce it with
    // nothing costs latency for no benefit.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(%error, "could not disable Nagle");
    }

    // A TLS record begins 0x16; an HTTP request begins with its method. The device may reach only this
    // port, so the simulated meter of `meter` is answered here rather than on a port of its own. Peeking
    // leaves the octet in the socket, so the handshake below still sees a complete stream.
    if crate::server::meter::METER.enabled() {
        let mut first = [0_u8; 1];
        match stream.peek(&mut first).await {
            Ok(1) if first.first() != Some(&0x16) => {
                crate::server::meter::METER.serve(stream, peer).await;
                return;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "could not peek at a new connection");
                return;
            }
        }
    }

    let stream = match acceptor.accept(stream).await {
        Ok(stream) => stream,
        Err(error) => {
            // The most likely cause is a certificate the device rejects, which is worth saying plainly
            // because the alternative reading — a network fault — sends people looking in the wrong place.
            tracing::warn!(
                %error,
                "TLS handshake failed; if this repeats, suspect the certificate rather than the network"
            );
            return;
        }
    };

    tracing::info!("TLS established");

    let mut session = Session::new(stream)
        .with_time_push(options.time_push)
        .with_cloud(options.cloud)
        .with_policy(options.policy)
        .with_recorder(options.recorder)
        .with_slots(options.slots)
        .with_registry(options.registry)
        .with_devices(options.devices);

    match session.run().await {
        Ok(stats) => tracing::info!(
            frames = stats.frames,
            telemetry = stats.telemetry,
            buffered = stats.buffered,
            reads = stats.reads,
            rejected = stats.rejected,
            undecoded = stats.undecoded,
            pings = stats.pings,
            relay_received = stats.relay_received,
            relay_dropped = stats.relay_dropped,
            refused_to_device = stats.refused_to_device,
            withheld_from_cloud = stats.withheld_from_cloud,
            "session ended"
        ),
        Err(error) => tracing::warn!(reason = %flatten(&error), "session failed"),
    }
}

/// Flatten an error and its sources into one line.
///
/// The point of choosing `snafu` was that context survives to the log; a bare `Display` prints only the
/// outermost layer and discards it.
fn flatten(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::{Devices, Peers, SessionOptions};
    use crate::growatt::cloud::{CloudConfig, CloudRelay};
    use crate::growatt::policy::Policy;
    use crate::mqtt::Trust;

    #[test]
    fn defaults_relay_nothing_record_nothing_and_push_time() {
        let options = SessionOptions::default();
        assert!(options.time_push, "the vendor server pushes time, so we do too");
        assert!(options.cloud.is_none(), "relaying is opt-in");
        assert!(options.recorder.is_none(), "recording is opt-in");
    }

    #[test]
    fn the_two_settings_are_independent_but_conventionally_opposed() {
        // The clock has one owner. Relaying with the cloud's configuration write passing through makes it
        // the cloud, so the wiring in `main` turns our push off. Nothing here enforces that — it is a
        // policy decision, and this type only carries it.
        let options = SessionOptions {
            time_push: false,
            cloud: Some(CloudRelay {
                endpoint: CloudConfig::default(),
                tls: Trust::BuiltIn.client_tls().expect("the shipped roots load"),
            }),
            policy: Policy::OPEN,
            recorder: None,
            slots: 1,
            registry: None,
            devices: Devices::default(),
        };
        assert!(options.cloud.is_some());
        assert!(!options.time_push);
    }

    #[test]
    fn nothing_is_filtered_until_a_list_says_so() {
        // Both allowlists are opt-in: the common case is one device on an isolated VLAN, which needs
        // neither, and a default that filtered would lock that device out on upgrade.
        let options = SessionOptions::default();
        assert!(options.devices.is_open(), "any serial is served");
        assert!(Peers::default().is_open(), "any address may connect");
        assert!(Peers::default().admits("203.0.113.9".parse().expect("an address")));
    }

    #[test]
    fn a_flattened_error_keeps_every_layer() {
        use crate::server::session::SessionError;

        // Two layers: the session's own message and the stream's underneath it. A bare Display would
        // print only the first, discarding the part that says what actually went wrong.
        let error = SessionError::Stream {
            source: crate::mqtt::StreamError::TooLarge {
                len: 99_999,
                limit: 65_536,
            },
        };
        let flat = super::flatten(&error);
        assert!(flat.contains("connection failed"), "{flat}");
        assert!(flat.contains("99999"), "{flat}");
    }
}
