//! The `heliobridge` binary: configuration, logging, shutdown. The work is in the library.
//!
//! This lives in `src/bin/heliobridge/` rather than `src/main.rs` so that a second binary is a second
//! directory, with no `Cargo.toml` change and no restructuring.

use std::process::ExitCode;

use heliobridge::VERSION;
use heliobridge::config::{Config, LogFormat};
use heliobridge::record::Recorder;
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;

    let cloud = config.cloud();
    if let Some(endpoint) = cloud.as_ref() {
        // Checked here so a bad host name fails at startup rather than on every reconnection attempt.
        endpoint
            .server_name()
            .map_err(|error| format!("cloud relay misconfigured: {}", chain(&error)))?;
        tracing::info!(
            cloud = %endpoint.address(),
            "relaying to the vendor cloud; the phone app and cloud integrations keep working"
        );
    }

    // Started inside the runtime: it spawns a writer task.
    let recorder = match config.recording() {
        Some(recording) => runtime
            .block_on(async { Recorder::start(recording) })
            .map(Some)
            .map_err(|error| format!("recording misconfigured: {}", chain(&error)))?,
        None => None,
    };

    let options = server::SessionOptions {
        time_push: config.should_push_time(),
        cloud,
        recorder,
    };
    if options.time_push {
        // The device is sent *local* time, so an operator whose host runs UTC — a container default —
        // would set the device's clock wrong by the zone offset, and nothing in the protocol would say
        // so. Naming the zone at startup makes the assumption visible before it matters.
        tracing::info!(
            local_time = %server::Clock::system().now(),
            tz = std::env::var("TZ").unwrap_or_else(|_| "<unset, using the system zone>".to_owned()),
            "will push this server's time to the device after it connects"
        );
    } else {
        tracing::info!("not pushing server time: the cloud relay is the clock authority");
    }

    let listen = config.listen;
    runtime.block_on(async move {
        let shutdown = async {
            match tokio::signal::ctrl_c().await {
                Ok(()) => tracing::info!("interrupt received"),
                Err(error) => tracing::error!(%error, "could not listen for an interrupt"),
            }
        };

        server::serve(listen, tls_config, options, shutdown)
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
