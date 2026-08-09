//! The task that keeps Home Assistant informed.
//!
//! It owns the broker connection and reacts to three things: the broker coming up, the set of connected
//! devices changing, and commands arriving on the command topic. Everything it publishes is derived from
//! the same [`crate::control`] state the socket API serves, so the two cannot disagree.
//!
//! # Availability is two topics
//!
//! There are two independent ways for readings to stop, and a broker can only notice one of them. That
//! this program died is a last will, published by the broker on our behalf. That a *device* stopped
//! arriving is something only this program can know, so it says so itself. Entities list both, which makes
//! them available exactly when both are true.

use core::time::Duration;

use tokio::sync::watch;

use crate::control::{Connected, Registry};
use crate::homeassistant::broker::{Broker, BrokerConfig, Event, Publication};
use crate::mqtt::{QoS, Will};

/// Payload for an availability topic when the thing is present.
pub const ONLINE: &[u8] = b"online";

/// Payload for an availability topic when it is not.
pub const OFFLINE: &[u8] = b"offline";

/// How the topics are named.
#[derive(Debug, Clone)]
pub struct Topics {
    /// Root for this program's own topics.
    pub base: String,
    /// Root Home Assistant watches for discovery.
    pub discovery_prefix: String,
}

impl Default for Topics {
    fn default() -> Self {
        Self {
            base: "heliobridge".to_owned(),
            discovery_prefix: "homeassistant".to_owned(),
        }
    }
}

impl Topics {
    /// Where this program reports whether *it* is running.
    ///
    /// Separate from any device, and the one topic that is a last will: the broker publishes it when this
    /// connection dies without a goodbye.
    pub fn bridge_availability(&self) -> String {
        format!("{}/bridge/availability", self.base)
    }

    /// Where a device's presence is reported.
    pub fn device_availability(&self, device: &str) -> String {
        format!("{}/{device}/availability", self.base)
    }

    /// Where a device's telemetry goes.
    pub fn state(&self, device: &str) -> String {
        format!("{}/{device}/state", self.base)
    }

    /// Where a device's settings go.
    pub fn settings(&self, device: &str) -> String {
        format!("{}/{device}/settings", self.base)
    }

    /// Where commands for any device arrive.
    ///
    /// One wildcard subscription rather than one per device: the broker does the matching, and a device
    /// connecting mid-session needs no new subscription.
    pub fn command_filter(&self) -> String {
        format!("{}/+/set", self.base)
    }

    /// The device a command topic addresses, or `None` if it is not one.
    pub fn device_of_command(&self, topic: &str) -> Option<String> {
        let rest = topic.strip_prefix(&self.base)?.strip_prefix('/')?;
        let device = rest.strip_suffix("/set")?;
        (!device.is_empty() && !device.contains('/')).then(|| device.to_owned())
    }

    /// The last will: what the broker publishes if this program stops without saying goodbye.
    pub fn will(&self) -> Will {
        Will {
            topic: self.bridge_availability(),
            payload: OFFLINE.to_vec(),
            qos: QoS::AtLeastOnce,
            // Retained, so a subscriber connecting afterwards learns the bridge is gone rather than
            // finding no availability at all and having to guess.
            retain: true,
        }
    }
}

/// Publishes device state to Home Assistant, and accepts commands back.
#[derive(Debug)]
pub struct Publisher {
    broker: Broker,
    topics: Topics,
    registry: Registry,
    devices: watch::Receiver<Connected>,
    /// Devices currently announced as online, so a change can be published as a difference.
    announced: Vec<String>,
}

impl Publisher {
    /// Connect to the broker and start publishing.
    ///
    /// # Errors
    ///
    /// Whatever [`Broker::connect`] reports. A broker that is merely unreachable is not an error: the
    /// client retries in the background.
    pub fn start(
        mut config: BrokerConfig,
        topics: Topics,
        registry: Registry,
    ) -> Result<Self, crate::homeassistant::broker::BrokerError> {
        config.will = Some(topics.will());
        config.subscriptions = vec![topics.command_filter()];

        let devices = registry.watch();
        Ok(Self {
            broker: Broker::connect(config)?,
            topics,
            registry,
            devices,
            announced: Vec::new(),
        })
    }

    /// Run until the broker client stops.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.broker.next_event() => {
                    let Some(event) = event else {
                        tracing::warn!("the broker client stopped; Home Assistant will not be updated");
                        return;
                    };
                    self.handle(event);
                }

                changed = self.devices.changed() => {
                    if changed.is_err() {
                        // The registry is gone, which means the program is shutting down.
                        return;
                    }
                    let connected = self.devices.borrow_and_update().clone();
                    self.announce_devices(&connected);
                }
            }
        }
    }

    /// React to something the broker reported.
    fn handle(&mut self, event: Event) {
        match event {
            Event::Connected => {
                // Everything retained is said again: a broker restart, a network blip and a first start
                // are then the same case, and there is no state to track about what it still holds.
                self.announced.clear();
                self.publish(Publication::retained(self.topics.bridge_availability(), ONLINE));
                let connected = self.devices.borrow().clone();
                self.announce_devices(&connected);
            }
            Event::Message { topic, payload } => {
                if let Some(device) = self.topics.device_of_command(&topic) {
                    self.handle_command(&device, &payload);
                } else {
                    tracing::debug!(%topic, "ignoring a message on an unexpected topic");
                }
            }
        }
    }

    /// Publish what changed in the connected set.
    fn announce_devices(&mut self, connected: &Connected) {
        for device in connected.devices() {
            if !self.announced.iter().any(|known| known == device) {
                tracing::info!(%device, "device is connected; announcing it to Home Assistant");
                self.publish(Publication::retained(self.topics.device_availability(device), ONLINE));
            }
        }
        // Anything that was announced and is no longer connected is reported gone, so entities go
        // unavailable rather than keeping their last value.
        let gone: Vec<String> = self
            .announced
            .iter()
            .filter(|device| !connected.contains(device))
            .cloned()
            .collect();
        for device in gone {
            tracing::info!(%device, "device is no longer connected");
            self.publish(Publication::retained(self.topics.device_availability(&device), OFFLINE));
        }
        self.announced = connected.devices().to_vec();
    }

    /// Carry out a command that arrived on a device's command topic.
    fn handle_command(&self, device: &str, payload: &[u8]) {
        if self.registry.handle(device).is_none() {
            tracing::warn!(%device, "a command arrived for a device that is not connected");
            return;
        }
        // Applying it belongs to the next step; recorded here so the subscription is visibly wired.
        tracing::info!(%device, bytes = payload.len(), "a command arrived for a device");
    }

    /// Queue a publication, noting when the broker cannot keep up.
    fn publish(&mut self, publication: Publication) {
        let topic = publication.topic.clone();
        if !self.broker.try_publish(publication) {
            tracing::warn!(%topic, dropped = self.broker.dropped(), "dropped a message: the broker is not keeping up");
        }
    }
}

/// How long to wait for the will to be registered before assuming the connection is usable.
///
/// Nothing waits on this yet; it exists so the value has one home when the first publish needs it.
pub const SETTLE: Duration = Duration::from_millis(250);

#[cfg(test)]
mod tests {
    use super::Topics;

    #[test]
    fn topics_are_built_from_one_base() {
        let topics = Topics::default();
        assert_eq!(topics.bridge_availability(), "heliobridge/bridge/availability");
        assert_eq!(
            topics.device_availability("0EXAMPLE00000001"),
            "heliobridge/0EXAMPLE00000001/availability"
        );
        assert_eq!(topics.state("0EXAMPLE00000001"), "heliobridge/0EXAMPLE00000001/state");
        assert_eq!(
            topics.settings("0EXAMPLE00000001"),
            "heliobridge/0EXAMPLE00000001/settings"
        );
        assert_eq!(topics.command_filter(), "heliobridge/+/set");
    }

    #[test]
    fn a_command_topic_names_its_device() {
        let topics = Topics::default();
        assert_eq!(
            topics.device_of_command("heliobridge/0EXAMPLE00000001/set").as_deref(),
            Some("0EXAMPLE00000001")
        );
    }

    #[test]
    fn anything_else_is_not_a_command_topic() {
        // The subscription is a wildcard, so what arrives is whatever the broker matched — including the
        // program's own state topics if a base were ever misconfigured to overlap.
        let topics = Topics::default();
        for topic in [
            "heliobridge/0EXAMPLE00000001/state",
            "heliobridge//set",
            "heliobridge/set",
            "elsewhere/0EXAMPLE00000001/set",
            "heliobridge/0EXAMPLE00000001/extra/set",
        ] {
            assert_eq!(topics.device_of_command(topic), None, "{topic}");
        }
    }

    #[test]
    fn the_will_is_retained_so_a_late_subscriber_learns_the_bridge_is_gone() {
        let will = Topics::default().will();
        assert_eq!(will.topic, "heliobridge/bridge/availability");
        assert_eq!(will.payload, b"offline");
        assert!(will.retain);
    }

    #[test]
    fn a_different_base_moves_every_topic_together() {
        let topics = Topics {
            base: "solar".to_owned(),
            discovery_prefix: "ha".to_owned(),
        };
        assert_eq!(topics.bridge_availability(), "solar/bridge/availability");
        assert_eq!(topics.command_filter(), "solar/+/set");
        assert_eq!(topics.device_of_command("solar/X/set").as_deref(), Some("X"));
        assert_eq!(topics.device_of_command("heliobridge/X/set"), None);
    }
}
