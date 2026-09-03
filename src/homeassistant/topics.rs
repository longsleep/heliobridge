//! What everything is called on the broker.
//!
//! One place, because the names have to agree across four things that are published separately — a
//! discovery message names the state topic, the state topic is written by a device task, the command topic
//! is subscribed to as a wildcard and parsed back into a serial, and the last will is registered before any
//! of them exists. A topic built by hand in two of those places is a bug that presents as an entity that
//! never updates.
//!
//! # Availability is two topics
//!
//! There are two independent ways for readings to stop, and a broker can only notice one of them. That
//! this program died is a last will, published by the broker on our behalf. That a *device* stopped
//! arriving is something only this program can know, so it says so itself. Most entities list both, which
//! makes them available exactly when both are true; the two that exist to *report* an outage list only the
//! bridge's, or they would vanish at the moment they became interesting.

use crate::homeassistant::entity::{Component, Entity, Presence, Source};
use crate::mqtt::{QoS, Will};

/// Payload for an availability topic when the thing is present.
pub const ONLINE: &[u8] = b"online";

/// Payload for an availability topic when it is not.
pub const OFFLINE: &[u8] = b"offline";

/// How the topics are named.
///
/// # One broker may carry several bridges
///
/// Nothing here assumes it is the only instance. Every device-facing topic and every entity identifier
/// carries the device serial, which is unique to the hardware, so two bridges serving different devices
/// never write to the same place — and two bridges serving the *same* device is a misconfiguration no
/// naming scheme can rescue.
///
/// The one topic that is not about a device is this program's own availability, so that one carries
/// [`Self::instance`] instead. A shared name there would be worse than useless: one bridge stopping would
/// mark another's entities unavailable.
#[derive(Debug, Clone)]
pub struct Topics {
    /// Root for this program's own topics.
    pub base: String,
    /// Root Home Assistant watches for discovery.
    pub discovery_prefix: String,
    /// What distinguishes this bridge from another on the same broker.
    ///
    /// Defaults to the host name, which is stable across restarts — an identifier that changed each time
    /// would leave a retained availability topic behind on every restart.
    pub instance: String,
}

impl Default for Topics {
    fn default() -> Self {
        Self {
            base: "heliobridge".to_owned(),
            discovery_prefix: "homeassistant".to_owned(),
            instance: default_instance(),
        }
    }
}

/// This host's name, or a fixed fallback where it cannot be read or is not usable in a topic.
pub fn default_instance() -> String {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let cleaned: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    if cleaned.is_empty() {
        "bridge".to_owned()
    } else {
        cleaned
    }
}

impl Topics {
    /// Where this program reports whether *it* is running.
    ///
    /// Scoped to this instance, and the one topic that is a last will: the broker publishes it when this
    /// connection dies without a goodbye.
    pub fn bridge_availability(&self) -> String {
        format!("{}/bridge/{}/availability", self.base, self.instance)
    }

    /// Where an entity's discovery message goes.
    ///
    /// `<prefix>/<component>/<node>/<object>/config`, with the base topic as the node so an object
    /// identifier cannot collide with another integration's, and the serial in the object so two devices
    /// cannot collide with each other.
    pub fn discovery(&self, component: Component, device: &str, key: &str) -> String {
        format!(
            "{}/{component}/{}/{device}_{key}/config",
            self.discovery_prefix, self.base
        )
    }

    /// The identifier Home Assistant remembers an entity by.
    ///
    /// It must be unique across everything the broker carries and stable across restarts, so it is built
    /// from the same parts as the discovery topic.
    pub fn unique_id(&self, device: &str, key: &str) -> String {
        format!("{}_{device}_{key}", self.base)
    }

    /// Where a device's presence is reported.
    pub fn device_availability(&self, device: &str) -> String {
        format!("{}/{device}/availability", self.base)
    }

    /// Where one entity's own presence is reported, for a setting another one can override.
    ///
    /// Per entity rather than per device: two slots can be in different work modes, so one slot's power
    /// setting may be effective while another's is not.
    pub fn entity_availability(&self, device: &str, key: &str) -> String {
        format!("{}/{device}/availability/{key}", self.base)
    }

    /// Which availability topics an entity depends on, in the order they go into its discovery message.
    ///
    /// Combined with `availability_mode: all`, so every topic listed must say online. A gated entity adds
    /// a third topic of its own, which this program drives from whatever its gate names.
    pub fn availability(&self, entity: &Entity, device: &str) -> Vec<String> {
        let mut topics = match entity.presence {
            Presence::Bridge => vec![self.bridge_availability()],
            Presence::Device => vec![self.bridge_availability(), self.device_availability(device)],
        };
        if entity.gate.is_some() {
            topics.push(self.entity_availability(device, entity.key));
        }
        topics
    }

    /// Where a device's telemetry goes.
    pub fn state(&self, device: &str) -> String {
        format!("{}/{device}/state", self.base)
    }

    /// Where a device's settings go.
    pub fn settings(&self, device: &str) -> String {
        format!("{}/{device}/settings", self.base)
    }

    /// Where what this bridge knows *about* a device goes: whether it is connected, and how stale its
    /// readings are.
    ///
    /// A third topic rather than a field in `state`, because it must keep being published when `state` is
    /// not. Its whole purpose is to describe the gap.
    pub fn status(&self, device: &str) -> String {
        format!("{}/{device}/status", self.base)
    }

    /// Where the datalogger's own configuration goes: signal, reporting interval, link diagnostics.
    ///
    /// Its own topic rather than fields in `state`, because it is a different address space arriving on a
    /// different schedule — once per connect, where telemetry arrives every five seconds.
    pub fn config(&self, device: &str) -> String {
        format!("{}/{device}/config", self.base)
    }

    /// Which topic carries a given kind of state.
    pub fn topic_for(&self, source: Source, device: &str) -> String {
        match source {
            Source::Telemetry => self.state(device),
            Source::Settings => self.settings(device),
            Source::Status => self.status(device),
            Source::Config => self.config(device),
        }
    }

    /// Where commands for any device arrive.
    ///
    /// One wildcard subscription rather than one per device: the broker does the matching, and a device
    /// connecting mid-session needs no new subscription.
    pub fn command_filter(&self) -> String {
        format!("{}/+/set", self.base)
    }

    /// Where commands for one device arrive, as a discovery message must name it.
    pub fn command(&self, device: &str) -> String {
        format!("{}/{device}/set", self.base)
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

#[cfg(test)]
mod tests {
    use super::Topics;
    use crate::growatt::v7::registers::{HoldingRegister, INPUT_REGISTERS, InputRegister};
    use crate::homeassistant::entity::{Component, Entity, Source};

    /// Two bridges on one broker, distinguished only by their instance name.
    fn two_instances() -> (Topics, Topics) {
        let first = Topics {
            instance: "attic".to_owned(),
            ..Topics::default()
        };
        let second = Topics {
            instance: "shed".to_owned(),
            ..Topics::default()
        };
        (first, second)
    }

    #[test]
    fn two_bridges_share_every_device_topic_but_not_their_own() {
        // A device topic is keyed by serial, so whichever bridge serves that device writes to the same
        // place — there is only ever one. Availability of the bridge itself is per instance: shared, one
        // bridge stopping would mark another's entities unavailable.
        let (attic, shed) = two_instances();

        assert_eq!(attic.state("0EXAMPLE00000001"), shed.state("0EXAMPLE00000001"));
        assert_eq!(
            attic.device_availability("0EXAMPLE00000001"),
            shed.device_availability("0EXAMPLE00000001")
        );
        assert_ne!(attic.bridge_availability(), shed.bridge_availability());
        assert_eq!(attic.bridge_availability(), "heliobridge/bridge/attic/availability");
    }

    #[test]
    fn each_bridge_wills_only_its_own_availability() {
        let (attic, shed) = two_instances();
        assert_eq!(attic.will().topic, attic.bridge_availability());
        assert_ne!(attic.will().topic, shed.will().topic);
    }

    #[test]
    fn entity_identity_is_the_devices_not_the_bridges() {
        // Two bridges must describe the same device identically: a unique_id that varied by instance
        // would make Home Assistant create a second set of entities after the device moved bridges.
        let (attic, shed) = two_instances();
        for topics in [&attic, &shed] {
            assert_eq!(
                topics.discovery(Component::Sensor, "0EXAMPLE00000001", "ac_power"),
                "homeassistant/sensor/heliobridge/0EXAMPLE00000001_ac_power/config"
            );
            assert_eq!(
                topics.unique_id("0EXAMPLE00000001", "ac_power"),
                "heliobridge_0EXAMPLE00000001_ac_power"
            );
        }
    }

    #[test]
    fn two_devices_never_share_an_identifier() {
        let topics = Topics::default();
        assert_ne!(
            topics.unique_id("0EXAMPLE00000001", "ac_power"),
            topics.unique_id("0EXAMPLE00000002", "ac_power")
        );
        assert_ne!(
            topics.discovery(Component::Sensor, "0EXAMPLE00000001", "ac_power"),
            topics.discovery(Component::Sensor, "0EXAMPLE00000002", "ac_power")
        );
    }

    #[test]
    fn the_default_instance_is_usable_in_a_topic() {
        // Whatever the host is called, the result has to be a legal topic segment: no slashes, no wildcard
        // characters, and never empty.
        let instance = super::default_instance();
        assert!(!instance.is_empty());
        assert!(
            !instance.contains(['/', '+', '#']),
            "unusable instance name: {instance}"
        );
    }

    #[test]
    fn topics_are_built_from_one_base() {
        let topics = Topics::default();
        assert_eq!(
            topics.bridge_availability(),
            format!("heliobridge/bridge/{}/availability", topics.instance)
        );
        assert_eq!(
            topics.device_availability("0EXAMPLE00000001"),
            "heliobridge/0EXAMPLE00000001/availability"
        );
        assert_eq!(topics.state("0EXAMPLE00000001"), "heliobridge/0EXAMPLE00000001/state");
        assert_eq!(
            topics.settings("0EXAMPLE00000001"),
            "heliobridge/0EXAMPLE00000001/settings"
        );
        assert_eq!(topics.status("0EXAMPLE00000001"), "heliobridge/0EXAMPLE00000001/status");
        assert_eq!(topics.command("0EXAMPLE00000001"), "heliobridge/0EXAMPLE00000001/set");
        assert_eq!(topics.command_filter(), "heliobridge/+/set");
    }

    #[test]
    fn each_kind_of_state_has_its_own_topic() {
        // Settings and telemetry are published on different schedules from different sources, and status
        // keeps being published when neither is. Sharing a topic would mean a settings publish overwriting
        // telemetry with an object missing every reading.
        let topics = Topics::default();
        let device = "0EXAMPLE00000001";
        let all = [Source::Telemetry, Source::Settings, Source::Status, Source::Config]
            .map(|source| topics.topic_for(source, device));
        assert_eq!(all[0], topics.state(device));
        assert_eq!(all[1], topics.settings(device));
        assert_eq!(all[2], topics.status(device));
        for (index, topic) in all.iter().enumerate() {
            assert!(!all.iter().skip(index.saturating_add(1)).any(|other| other == topic));
        }
    }

    /// An ordinary telemetry register, for the availability tests.
    fn ac_power() -> InputRegister {
        *INPUT_REGISTERS
            .iter()
            .find(|entry| entry.name == "ac_power")
            .expect("ac_power is mapped")
    }

    #[test]
    fn the_entities_that_report_an_outage_do_not_depend_on_the_device() {
        // The point of the distinction: an entity listing the device's availability goes unavailable
        // exactly when the device does, which is useless for a sensor whose job is to say so.
        let topics = Topics::default();
        let device = "0EXAMPLE00000001";
        assert_eq!(
            topics.availability(&Entity::for_reading(&ac_power()).expect("an entity"), device),
            vec![topics.bridge_availability(), topics.device_availability(device)]
        );
        assert_eq!(
            topics.availability(&Entity::connected(), device),
            vec![topics.bridge_availability()]
        );
    }

    #[test]
    fn an_overridable_setting_adds_a_topic_of_its_own() {
        // Per entity, so one slot going into smart self-use does not take another slot's power setting
        // away with it.
        let topics = Topics::default();
        let device = "0EXAMPLE00000001";
        let power = Entity::for_setting(&HoldingRegister::slot(1).expect("slot 1")[3]);
        assert!(power.gate.is_some(), "the slot power setting is gated by its work mode");
        assert_eq!(
            topics.availability(&power, device),
            vec![
                topics.bridge_availability(),
                topics.device_availability(device),
                topics.entity_availability(device, "slot1_output_power"),
            ]
        );
        assert_ne!(
            topics.entity_availability(device, "slot1_output_power"),
            topics.entity_availability(device, "slot2_output_power")
        );
    }

    #[test]
    fn a_command_topic_names_its_device() {
        let topics = Topics::default();
        assert_eq!(
            topics.device_of_command("heliobridge/0EXAMPLE00000001/set").as_deref(),
            Some("0EXAMPLE00000001")
        );
        // What a discovery message tells Home Assistant to write to must be what the wildcard matches.
        assert_eq!(
            topics.device_of_command(&topics.command("0EXAMPLE00000001")).as_deref(),
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
        let topics = Topics::default();
        let will = topics.will();
        assert_eq!(will.topic, topics.bridge_availability());
        assert_eq!(will.payload, b"offline");
        assert!(will.retain);
    }

    #[test]
    fn a_different_base_moves_every_topic_together() {
        let topics = Topics {
            base: "solar".to_owned(),
            discovery_prefix: "ha".to_owned(),
            instance: "roof".to_owned(),
        };
        assert_eq!(topics.bridge_availability(), "solar/bridge/roof/availability");
        assert_eq!(topics.command_filter(), "solar/+/set");
        assert_eq!(topics.device_of_command("solar/X/set").as_deref(), Some("X"));
        assert_eq!(topics.device_of_command("heliobridge/X/set"), None);
        assert_eq!(
            topics.discovery(Component::Sensor, "X", "ac_power"),
            "ha/sensor/solar/X_ac_power/config"
        );
    }
}
