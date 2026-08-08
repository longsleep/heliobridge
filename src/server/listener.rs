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

use crate::growatt::cloud::CloudConfig;
use crate::record::Recorder;
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
    /// Off when relaying to the cloud, which sends its own — two servers setting one clock would set it
    /// twice per connect, to values differing by whatever skew exists between them.
    pub time_push: bool,

    /// Relay traffic to the vendor cloud, so the phone app keeps working.
    pub cloud: Option<CloudConfig>,

    /// Record every frame, in both directions plus the ones this program originates.
    pub recorder: Option<Recorder>,

    /// How many schedule slots to read back at startup.
    pub slots: u16,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            time_push: true,
            cloud: None,
            recorder: None,
            slots: 1,
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
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ListenerError> {
    let listener = TcpListener::bind(address).await.context(BindSnafu { address })?;
    let acceptor = TlsAcceptor::from(tls);

    tracing::info!(
        %address,
        time_push = options.time_push,
        cloud_relay = options.cloud.is_some(),
        recording = options.recorder.is_some(),
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
        .with_recorder(options.recorder)
        .with_slots(options.slots);

    match session.run().await {
        Ok(stats) => tracing::info!(
            frames = stats.frames,
            telemetry = stats.telemetry,
            reads = stats.reads,
            rejected = stats.rejected,
            undecoded = stats.undecoded,
            pings = stats.pings,
            relay_received = stats.relay_received,
            relay_dropped = stats.relay_dropped,
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
    use super::SessionOptions;
    use crate::growatt::cloud::CloudConfig;

    #[test]
    fn defaults_relay_nothing_record_nothing_and_push_time() {
        let options = SessionOptions::default();
        assert!(options.time_push, "the vendor server pushes time, so we do too");
        assert!(options.cloud.is_none(), "relaying is opt-in");
        assert!(options.recorder.is_none(), "recording is opt-in");
    }

    #[test]
    fn the_two_settings_are_independent_but_conventionally_opposed() {
        // Relaying means the cloud owns the clock, so the wiring in `main` turns the push off. Nothing
        // here enforces that — it is a policy decision, and this type only carries it.
        let options = SessionOptions {
            time_push: false,
            cloud: Some(CloudConfig::default()),
            recorder: None,
            slots: 1,
        };
        assert!(options.cloud.is_some());
        assert!(!options.time_push);
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
