//! The manufacturer's own cloud: where it is, and what it takes to stand in front of it.
//!
//! This program can keep the device's cloud connection working by relaying between the two. What that
//! costs is knowledge no server can have on its own: which host a device of this make dials, what it sends
//! to authenticate, which topics carry what, and — because the device is talking to *this* program
//! instead — which names the certificate it is offered must carry.
//!
//! # What is shared and what is not
//!
//! [`Message`] and [`Endpoint`] are defined here because they are the currency crossing the seam, and both
//! are transport vocabulary rather than any one manufacturer's: a topic, a delivery guarantee, a payload,
//! a host and a port. Everything with a make's name in it — the address, the credentials, the keepalive
//! the cloud expects, the spelling of the uplink topic — stays behind [`Upstream`].
//!
//! # The relay is a handle, not a connection
//!
//! [`Upstream::relay`] hands back something that can be published to and polled, and dropping it stops
//! the relay. Whether that is a task, a socket or a queue is the driver's business; a session only needs
//! somewhere to put a frame and somewhere to get one.

use std::future::Future;
use std::net::SocketAddr;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use rustls::pki_types::ServerName;

use crate::mqtt::{ClientTls, QoS};

use super::wire::Wire;

/// A host that cannot be a TLS server name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{host:?} cannot be a TLS server name")]
pub struct InvalidHost {
    /// The host as configured.
    pub host: String,
}

/// Where to reach a cloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Host name. Also the TLS server name, so it must be a name rather than an address.
    pub host: String,
    /// Port.
    pub port: u16,
}

impl Endpoint {
    /// `host:port`, for connecting and for logs.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The host as a TLS server name.
    ///
    /// # Errors
    ///
    /// [`InvalidHost`] if it cannot be one — worth checking up front, so a misconfiguration is reported at
    /// startup rather than on every reconnection attempt.
    pub fn server_name(&self) -> Result<ServerName<'static>, InvalidHost> {
        ServerName::try_from(self.host.clone()).map_err(|_| InvalidHost {
            host: self.host.clone(),
        })
    }
}

/// How a relay finds the cloud's addresses.
///
/// The system's resolver unless the operator names others. That matters on a network whose DNS override
/// points the cloud's name at this program: asking the resolver the device asks would dial this program
/// back instead of the cloud. Plain DNS is enough, because the certificate the cloud presents is still
/// checked against its name — a forged answer can fail a connection but cannot make the relay trust anyone
/// else.
#[derive(Clone)]
pub struct Resolver {
    servers: Vec<SocketAddr>,
    through: Option<TokioResolver>,
}

impl Resolver {
    /// The system's resolver.
    pub const fn system() -> Self {
        Self {
            servers: Vec::new(),
            through: None,
        }
    }

    /// These name servers, asked over UDP with TCP for a truncated answer, in order. None is the system's.
    ///
    /// # Errors
    ///
    /// The resolver's own refusal to be built, as text.
    pub fn through(servers: &[SocketAddr]) -> Result<Self, String> {
        if servers.is_empty() {
            return Ok(Self::system());
        }
        let name_servers = servers
            .iter()
            .map(|server| {
                let mut config = NameServerConfig::udp_and_tcp(server.ip());
                for connection in &mut config.connections {
                    connection.port = server.port();
                }
                config
            })
            .collect();
        let resolver = TokioResolver::builder_with_config(
            ResolverConfig::from_name_servers(name_servers),
            TokioRuntimeProvider::default(),
        )
        .build()
        .map_err(|error| error.to_string())?;
        Ok(Self {
            servers: servers.to_vec(),
            through: Some(resolver),
        })
    }

    /// The name servers asked, empty for the system's resolver.
    pub fn servers(&self) -> &[SocketAddr] {
        &self.servers
    }

    /// Every address the endpoint's host resolves to, with its port.
    ///
    /// # Errors
    ///
    /// The lookup's failure, as text for a log line.
    pub async fn addresses(&self, endpoint: &Endpoint) -> Result<Vec<SocketAddr>, String> {
        match &self.through {
            None => tokio::net::lookup_host(endpoint.address())
                .await
                .map(Iterator::collect)
                .map_err(|error| error.to_string()),
            Some(resolver) => {
                let found = resolver
                    .lookup_ip(endpoint.host.as_str())
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(found.iter().map(|ip| SocketAddr::new(ip, endpoint.port)).collect())
            }
        }
    }
}

impl core::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Resolver")
            .field("servers", &self.servers)
            .finish_non_exhaustive()
    }
}

/// Everything a relay needs: where to reach the cloud, how to find it, and whom to trust when doing it.
///
/// They travel together because a relay cannot be started without them, and because the TLS configuration
/// and the resolver are built once for the process — pairing them here is what stops a caller from
/// accidentally building a second one per device connection.
#[derive(Debug, Clone)]
pub struct Target {
    /// The endpoint to dial.
    pub endpoint: Endpoint,
    /// The process's one outbound TLS configuration.
    pub tls: ClientTls,
    /// How the endpoint's host is looked up.
    pub resolver: Resolver,
}

/// One publish, moving in either direction through a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Topic exactly as it appeared on the hop it came from. Empty means the device's uplink topic, which
    /// only the driver knows how to spell.
    pub topic: String,
    /// Delivery guarantee, preserved so the far side sees what the near side sent.
    pub qos: QoS,
    /// The protocol frame, untouched.
    pub payload: Vec<u8>,
}

impl Message {
    /// A device frame headed for the cloud, on the default uplink topic.
    pub const fn uplink(payload: Vec<u8>, qos: QoS) -> Self {
        Self {
            topic: String::new(),
            qos,
            payload,
        }
    }
}

/// A running relay, for one device session.
///
/// Dropping it stops the relay: a session's lifetime is its relay's lifetime.
pub trait Relay: std::fmt::Debug + Send + 'static {
    /// Hand a message to the cloud, without waiting.
    ///
    /// Returns whether it was queued. Refusing is the right answer when the cloud is not keeping up: the
    /// device is waiting on this program, not on a manufacturer's servers.
    fn try_forward(&mut self, message: Message) -> bool;

    /// The next message the cloud sent for the device.
    ///
    /// Resolves to `None` once the relay has stopped, after which a session should stop polling. The
    /// returned future must be `Send`, because a session is polled on a spawned task.
    fn next_from_cloud(&mut self) -> impl Future<Output = Option<Message>> + Send;
}

/// Reaching the manufacturer's cloud, and being mistaken for it.
pub trait Upstream: Wire {
    /// This driver's running relay.
    type Relay: Relay;

    /// Why a relay could not be started.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Where a device of this make connects when nothing overrides it.
    fn endpoint(&self) -> Endpoint;

    /// Names a certificate presented to the device must carry, common name first.
    ///
    /// The device is dialing its manufacturer's host name and getting this program; the certificate has to
    /// look like the one it expected, whether or not this particular device checks.
    fn certificate_names(&self) -> &'static [&'static str];

    /// Start relaying for one device session.
    ///
    /// # Errors
    ///
    /// [`Self::Error`] if the relay cannot be started at all. Failing to *connect* is not that: a driver
    /// is expected to keep retrying while the session continues either way.
    fn relay(&self, device_id: &str, target: Target) -> Result<Self::Relay, Self::Error>;
}

/// A relay that carries nothing, for a driver with no cloud of its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRelay;

impl Relay for NoRelay {
    fn try_forward(&mut self, _message: Message) -> bool {
        false
    }

    /// Never resolves, rather than resolving to `None`: a session reads `None` as "the relay stopped" and
    /// ends itself, which would be the wrong conclusion when there was never a relay to stop.
    fn next_from_cloud(&mut self) -> impl Future<Output = Option<Message>> + Send {
        std::future::pending()
    }
}

/// A driver that has no cloud to relay to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this driver has no upstream cloud")]
pub struct NoUpstream;
