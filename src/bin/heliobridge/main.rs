//! The `heliobridge` binary: configuration, logging, shutdown. The work is in the library.
//!
//! This lives in `src/bin/heliobridge/` rather than `src/main.rs` so that a second binary is a second
//! directory, with no `Cargo.toml` change and no restructuring.

use std::process::ExitCode;

use heliobridge::VERSION;
use heliobridge::config::{Config, LogFormat};
use heliobridge::server;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

fn main() -> ExitCode {
    let config = Config::from_env();
    init_tracing(&config);

    match run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Everything after logging is up, so failures can be reported through tracing rather than stderr.
fn run(config: &Config) -> Result<(), String> {
    tracing::info!(version = VERSION, "heliobridge starting");

    let pair = config.tls_pair().map_err(str::to_owned)?;
    let (cert, key) = match pair {
        Some((cert, key)) => (Some(cert.as_path()), Some(key.as_path())),
        None => (None, None),
    };

    let (tls_config, origin) = server::server_config(cert, key, &config.state_dir)
        .map_err(|error| format!("TLS setup failed: {}", chain(&error)))?;
    tracing::info!(%origin, state_dir = %config.state_dir.display(), "certificate ready");

    if let Some(dir) = config.record_dir.as_ref() {
        // Not implemented yet; say so rather than let an operator believe frames are being captured.
        tracing::warn!(
            dir = %dir.display(),
            "frame recording is configured but not implemented in this build; ignoring"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;

    let listen = config.listen;
    runtime.block_on(async move {
        let shutdown = async {
            match tokio::signal::ctrl_c().await {
                Ok(()) => tracing::info!("interrupt received"),
                Err(error) => tracing::error!(%error, "could not listen for an interrupt"),
            }
        };

        server::serve(listen, tls_config, shutdown)
            .await
            .map_err(|error| format!("listener failed: {}", chain(&error)))
    })?;

    tracing::info!("stopped");
    Ok(())
}

/// Flatten an error and its sources into one line.
///
/// The whole point of choosing `snafu` was that context survives to the log; a bare `Display` prints
/// only the outermost layer and discards it.
fn chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}

/// Honour `HELIOBRIDGE_LOG` first, then `RUST_LOG`, then default to `info`.
fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_new(&config.log)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);
    match config.log_format {
        LogFormat::Text => registry.with(tracing_subscriber::fmt::layer()).init(),
        LogFormat::Json => registry.with(tracing_subscriber::fmt::layer().json()).init(),
    }
}
