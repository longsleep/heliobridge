//! The `heliobridge` binary: configuration, logging, shutdown. The work is in the library.
//!
//! This lives in `src/bin/heliobridge/` rather than `src/main.rs` so that a second binary is a second
//! directory, with no `Cargo.toml` change and no restructuring.

use std::process::ExitCode;
use std::sync::Arc;

use heliobridge::VERSION;
use heliobridge::config::{Command, Config, LogFormat};
use heliobridge::control::{self, Registry};
use heliobridge::driver::Driver;
use heliobridge::driver::upstream::Target;
use heliobridge::growatt::driver::Growatt;
use heliobridge::homeassistant::broker::{BrokerConfig, BrokerUrl};
use heliobridge::homeassistant::command;
use heliobridge::homeassistant::publisher::Publisher;
use heliobridge::mqtt::{ClientTls, Trust};
use heliobridge::record::Recorder;
use heliobridge::server;
use heliobridge::server::firmware::FirmwareStore;
use rustls::ServerConfig;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

fn main() -> ExitCode {
    let config = Config::from_env();

    init_tracing(&config);

    // A question with an exit code rather than a program that serves. Answered through tracing like
    // everything else, so a failure reaches wherever the deployment already collects output.
    if config.command == Some(Command::Healthz) {
        return healthz(&config);
    }

    match run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Ask a running server whether it is alive.
///
/// A single-threaded runtime: one connection, one request, one answer, and the process exits — a worker
/// per core would cost more to start than the check costs to run.
fn healthz(config: &Config) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start the async runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(server::probe::Check::for_listener(config.listen).run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Everything after logging is up, so failures can be reported through tracing rather than stderr.
fn run(config: &Config) -> Result<(), String> {
    tracing::info!(version = VERSION, "heliobridge starting");

    // The one line in the program that names a manufacturer. Everything the bridge assembles below takes
    // this as a `Driver` and cannot tell which one it got.
    Bridge::new(config, Arc::new(Growatt))?
        .with_cloud_relay()?
        .with_recording()?
        .with_control_api()?
        .with_home_assistant()?
        .serve()
}

/// The program's parts, assembled in dependency order.
///
/// Each step needs what the ones before it produced — the relay needs the outbound TLS configuration, the
/// Home Assistant publisher needs the registry the control API may already have created — so they are
/// methods over shared state rather than functions passing it along. What is optional stays `Option`, and
/// a step that is switched off is a method that does nothing.
struct Bridge<'a, D: Driver> {
    config: &'a Config,
    /// What the bytes mean, whoever made the device. Chosen by the caller and passed on unopened: this
    /// type knows only what [`heliobridge::driver`] says a driver can do.
    driver: Arc<D>,
    /// Runs every task. The recorder, the control API and the publisher all spawn into it, so it must
    /// exist before any of them.
    runtime: tokio::runtime::Runtime,
    /// The certificate presented to the device.
    server_tls: Arc<ServerConfig>,
    /// What everything this program dials trusts.
    client_tls: ClientTls,
    cloud: Option<Target>,
    recorder: Option<Recorder>,
    registry: Option<Registry>,
}

impl<'a, D: Driver> Bridge<'a, D> {
    /// The parts that are not optional: a runtime, a certificate to present, and anchors to trust.
    ///
    /// The certificate is the driver's business too: a device dialing its manufacturer's host name and
    /// reaching this program has to be offered something that looks like what it expected.
    fn new(config: &'a Config, driver: Arc<D>) -> Result<Self, String> {
        let pair = config.tls_pair().map_err(str::to_owned)?;
        let (cert, key) = match pair {
            Some((cert, key)) => (Some(cert.as_path()), Some(key.as_path())),
            None => (None, None),
        };

        let (server_tls, origin) = server::server_config(cert, key, &config.state_dir, driver.certificate_names())
            .map_err(|error| format!("TLS setup failed: {}", chain(&error)))?;
        tracing::info!(%origin, state_dir = %config.state_dir.display(), "certificate ready");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not start the async runtime: {error}"))?;

        // Loaded here so a trust store the operator named but that cannot be read is reported at startup,
        // not on every reconnection attempt.
        let client_tls = ClientTls::from_env().map_err(|error| format!("outbound TLS: {}", chain(&error)))?;

        Ok(Self {
            config,
            driver,
            runtime,
            server_tls,
            client_tls,
            cloud: None,
            recorder: None,
            registry: None,
        })
    }

    /// Relay to the vendor cloud, when asked for.
    fn with_cloud_relay(mut self) -> Result<Self, String> {
        let Some(endpoint) = self.config.cloud(self.driver.endpoint()) else {
            return Ok(self);
        };

        // Checked here so a bad host name fails at startup rather than on every reconnection attempt.
        endpoint
            .server_name()
            .map_err(|error| format!("cloud relay misconfigured: {}", chain(&error)))?;
        tracing::info!(
            cloud = %endpoint.address(),
            "relaying to the vendor cloud; the phone app and cloud integrations keep working"
        );
        // Worth a line of its own: the relay is the one place where a party other than this program can
        // write to the device, so what it will and will not carry should not have to be inferred.
        tracing::info!(
            mode = ?self.config.relay_mode,
            answers = ?self.config.relay_answers,
            "relay policy: how much the cloud may change, and which command answers it is told about. \
             Telemetry, identity and settings snapshots are always forwarded"
        );

        self.cloud = Some(Target {
            endpoint,
            tls: self.client_tls.clone(),
        });
        Ok(self)
    }

    /// Record every frame, when asked for.
    fn with_recording(mut self) -> Result<Self, String> {
        let Some(recording) = self.config.recording() else {
            return Ok(self);
        };
        // Started inside the runtime: it spawns a writer task.
        self.recorder = self
            .runtime
            .block_on(async { Recorder::start(recording) })
            .map(Some)
            .map_err(|error| format!("recording misconfigured: {}", chain(&error)))?;
        Ok(self)
    }

    /// Serve the control socket, when asked for.
    fn with_control_api(mut self) -> Result<Self, String> {
        let Some(path) = self.config.control_socket.as_deref() else {
            return Ok(self);
        };

        let registry = self.registry.take().unwrap_or_default();
        // Started inside the runtime for the same reason as the recorder: it spawns tasks.
        self.runtime
            .block_on(async { control::listen(path, registry.clone(), Arc::clone(&self.driver)) })
            .map_err(|error| format!("control API failed to start: {}", chain(&error)))?;
        tracing::info!(
            socket = %path.display(),
            "control API enabled; settings can be written through it"
        );

        self.registry = Some(registry);
        Ok(self)
    }

    /// Publish to Home Assistant, when a broker is configured.
    ///
    /// Shares the control API's registry where there is one, so both interfaces address the same sessions.
    /// The broker being down is not a startup failure — the client retries — so only a configuration
    /// problem stops the program here.
    fn with_home_assistant(mut self) -> Result<Self, String> {
        let Some(url) = self.config.mqtt_url.as_deref() else {
            return Ok(self);
        };

        let broker = BrokerConfig {
            url: BrokerUrl::parse(url).map_err(|error| format!("broker URL: {}", chain(&error)))?,
            // The process identifier keeps two instances on one host from evicting each other: a broker
            // disconnects the older client when a second presents the same identifier.
            client_id: format!("heliobridge-{}", std::process::id()),
            username: self.config.mqtt_user.clone(),
            password: self.config.mqtt_password()?,
            subscriptions: Vec::new(),
            will: None,
            tls: self.broker_tls()?,
        };
        tracing::info!(
            broker = %broker.url,
            authenticated = broker.username.is_some(),
            "publishing to Home Assistant"
        );

        let topics = self.config.topics();
        let options = self.config.publishing();
        tracing::info!(
            base = %topics.base,
            discovery_prefix = %topics.discovery_prefix,
            instance = %topics.instance,
            slots = options.slots,
            writable = options.permitted.writes,
            offline_after_s = options.offline_after.as_secs(),
            "Home Assistant topics"
        );
        // Each is worth its own line: a setting that silently will not move is the kind of thing someone
        // spends an afternoon on before checking the configuration.
        if !options.permitted.writes {
            tracing::info!("writes are refused: every setting is published as a read-only sensor");
        } else if !options.permitted.power_plus {
            tracing::info!(
                setting = command::POWER_PLUS,
                "this setting is published as a read-only sensor and commands naming it are refused"
            );
        }

        let registry = self.registry.take().unwrap_or_default();
        let publisher = self
            .runtime
            .block_on(async { Publisher::start(broker, topics, registry.clone(), options, Arc::clone(&self.driver)) })
            .map_err(|error| format!("broker: {}", chain(&error)))?;
        self.runtime.spawn(publisher.run());

        self.registry = Some(registry);
        Ok(self)
    }

    /// The TLS configuration for the broker.
    ///
    /// The shared one, unless the broker authenticates by certificate — then a second configuration over
    /// the same trust anchors, differing only in presenting an identity. That identity is deliberately not
    /// given to the cloud relay, which was never asked for one.
    fn broker_tls(&self) -> Result<ClientTls, String> {
        let Some((certificate, key)) = self.config.mqtt_client_identity()? else {
            return Ok(self.client_tls.clone());
        };

        let identity = server::client_identity(&certificate, &key)
            .map_err(|error| format!("broker client certificate: {}", chain(&error)))?;
        let tls = Trust::configured()
            .client_tls_with(Some(identity))
            .map_err(|error| format!("broker client certificate: {}", chain(&error)))?;
        tracing::info!(
            certificate = %certificate.display(),
            "authenticating to the broker with a client certificate"
        );
        Ok(tls)
    }

    /// Serve the device until interrupted.
    fn serve(self) -> Result<(), String> {
        // Read before binding, so a mistyped allowlist fails at startup rather than after the device has
        // already been refused for an hour.
        let peers = self.config.peers()?;
        // Also read before binding: asking to fetch firmware with nowhere to put it is a startup mistake.
        // This is where the vendor implementation meets the settings — composition belongs in `main`, and
        // it is the only place in the program that names both halves. The store exists either way, so an
        // advertisement is logged in a default configuration too; keeping one is what the settings add.
        let firmware = Some(match self.config.firmware()? {
            Some(settings) => {
                tracing::info!(
                    dir = %settings.dir.display(),
                    fetch = settings.fetch,
                    max_bytes = settings.max_bytes,
                    "firmware the cloud advertises will be logged and kept{}",
                    if settings.fetch { ", downloading it" } else { " once fetching is enabled" }
                );
                FirmwareStore::new(Arc::clone(&self.driver))
                    .keeping(settings.dir, settings.fetch, settings.max_bytes)
                    .map_err(|error| format!("firmware directory unusable: {}", chain(&error)))?
            }
            None => FirmwareStore::new(Arc::clone(&self.driver)),
        });
        let options = server::SessionOptions {
            driver: Arc::clone(&self.driver),
            time_push: self.config.should_push_time(),
            cloud: self.cloud,
            policy: self.config.policy(),
            recorder: self.recorder,
            firmware,
            slots: self.config.slots,
            registry: self.registry,
            devices: self.config.devices(),
        };
        if !peers.is_open() || !options.devices.is_open() {
            tracing::info!(
                accept_from = %peers,
                serve_devices = %options.devices,
                "connections are filtered; anything else is refused"
            );
        }
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
            tracing::info!("not pushing server time: relaying in full mode, so the cloud is the clock authority");
        }

        let listen = self.config.listen;
        let server_tls = self.server_tls;
        self.runtime.block_on(async move {
            let shutdown = async {
                // Both signals, because the two ways this program is stopped send different ones: a
                // terminal sends SIGINT, and every service manager sends SIGTERM. Handling only the
                // first means an orderly stop under systemd or Docker is the default action instead —
                // killed mid-write, with buffered frames lost from the recording and the device left
                // to time out rather than told the session ended.
                use tokio::signal::unix::{SignalKind, signal};

                let mut terminate = match signal(SignalKind::terminate()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        // Not fatal: losing the orderly path is worse than losing one of two ways to
                        // reach it, so fall back to interrupt alone rather than refusing to serve.
                        tracing::error!(%error, "could not listen for SIGTERM; only SIGINT will stop this cleanly");
                        match tokio::signal::ctrl_c().await {
                            Ok(()) => tracing::info!(signal = "SIGINT", "shutdown signal received"),
                            Err(error) => tracing::error!(%error, "could not listen for an interrupt"),
                        }
                        return;
                    }
                };

                tokio::select! {
                    result = tokio::signal::ctrl_c() => match result {
                        Ok(()) => tracing::info!(signal = "SIGINT", "shutdown signal received"),
                        Err(error) => tracing::error!(%error, "could not listen for an interrupt"),
                    },
                    _ = terminate.recv() => tracing::info!(signal = "SIGTERM", "shutdown signal received"),
                }
            };

            server::serve(listen, server_tls, options, peers, shutdown)
                .await
                .map_err(|error| format!("listener failed: {}", chain(&error)))
        })?;

        tracing::info!("stopped");
        Ok(())
    }
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
