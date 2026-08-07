//! The device-facing TLS listener.
//!
//! Accepts connections, wraps them in TLS and hands each to a [`crate::server::Session`]. One device is
//! expected, but connections are handled concurrently anyway: the device reconnects aggressively, and a
//! stale socket must not be able to lock out the live one.

use core::future::Future;
use core::pin::pin;
use std::sync::Arc;

use rustls::ServerConfig;
use snafu::{ResultExt, Snafu};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

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

    /// Accepting a connection failed.
    #[snafu(display("could not accept a connection"))]
    Accept {
        /// The underlying error.
        source: std::io::Error,
    },
}

/// Serve until the shutdown signal fires.
///
/// # Errors
///
/// [`ListenerError::Bind`] if the address is unavailable. Per-connection failures are logged and do not
/// stop the listener: the device has nowhere else to publish, so availability outranks strictness.
pub async fn serve(
    address: std::net::SocketAddr,
    tls: Arc<ServerConfig>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ListenerError> {
    let listener = TcpListener::bind(address).await.context(BindSnafu { address })?;
    let acceptor = TlsAcceptor::from(tls);

    tracing::info!(%address, "listening for the device");

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
                tokio::spawn(async move {
                    handle(stream, peer, acceptor).await;
                });
            }
        }
    }
}

/// Complete the TLS handshake and run one session.
#[tracing::instrument(skip(stream, acceptor), fields(%peer))]
async fn handle(stream: TcpStream, peer: std::net::SocketAddr, acceptor: TlsAcceptor) {
    // Nagle off: the device waits for small acknowledgements, and delaying a PUBACK to coalesce it with
    // nothing costs latency for no benefit.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(%error, "could not disable Nagle");
    }

    let stream = match acceptor.accept(stream).await {
        Ok(stream) => stream,
        Err(error) => {
            // The most likely cause is a certificate the device rejects, which is worth saying plainly
            // because the alternative reading — a network fault — sends people looking in the wrong
            // place.
            tracing::warn!(
                %error,
                "TLS handshake failed; if this repeats, suspect the certificate rather than the network"
            );
            return;
        }
    };

    tracing::info!("TLS established");

    let mut session = Session::new(stream);
    match session.run().await {
        Ok(stats) => tracing::info!(
            frames = stats.frames,
            telemetry = stats.telemetry,
            rejected = stats.rejected,
            undecoded = stats.undecoded,
            pings = stats.pings,
            "session ended"
        ),
        Err(error) => {
            let mut chain = vec![error.to_string()];
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                chain.push(cause.to_string());
                source = cause.source();
            }
            tracing::warn!(error = %chain.join(": "), "session failed");
        }
    }
}
