//! The retained messages that tell Home Assistant what exists.
//!
//! One message per entity, published under the discovery prefix and kept by the broker, so Home Assistant
//! finds the device whenever it starts rather than only while this program happens to be publishing. They
//! are republished on every broker connection, which makes a broker restart, a network blip and a first
//! start one case instead of three.
//!
//! # Every entity reads one field out of a shared object
//!
//! The alternative — a topic per field — is thirty publishes per telemetry cycle instead of one. So a
//! device publishes a JSON object and each entity carries a `value_template` picking its own field out of
//! it. A field that is missing renders empty, and Home Assistant leaves the previous reading in place
//! rather than blanking it — so a short frame loses nothing.
//!
//! **It does not do that silently.** A `text` entity fails the length and pattern it was announced with,
//! and a `sensor` logs `Invalid state message ''` — both observed, and both merely logged, so the state is
//! kept either way. A field that is only *sometimes* absent therefore gets a topic of its own, where
//! nothing is published until there is something to publish; see [`Entity::published_alone`]. A field that
//! is absent only in a malformed frame keeps sharing, since the alternative is a topic per register.

use serde_json::{Map, Value, json};

use crate::control::IdentityView;
use crate::driver::describes::Describes;
use crate::homeassistant::broker::Publication;
use crate::homeassistant::entity::{Bounds, Component, Entity, Shape};
use crate::homeassistant::topics::{OFFLINE, ONLINE, Topics};

/// Pattern a slot boundary must match, for the `text` entities that stand in for a missing `time`
/// component.
pub const TIME_PATTERN: &str = r"^([01]\d|2[0-3]):[0-5]\d$";

/// What Home Assistant groups the entities under.
///
/// Assembled from the identity report rather than stored, so it says what the device says. Everything but
/// the serial is optional: the report arrives about five seconds after the session starts, and a device
/// page that exists is worth more than one that waits for it. It is re-announced when the report lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceBlock {
    /// The serial, which is the identifier Home Assistant keys the device on.
    pub serial: String,
    /// The product, from the type code the report carries.
    /// What the driver calls this product, or `None` when it does not recognise the code.
    pub product: Option<&'static str>,
    /// Model as the datalogger reports it.
    pub model: Option<String>,
    /// Firmware version.
    pub firmware: Option<String>,
    /// Hardware version.
    pub hardware: Option<String>,
}

impl DeviceBlock {
    /// Describe a device from its identity report, if one has arrived yet.
    pub fn new(serial: &str, identity: Option<&IdentityView>, driver: &impl Describes) -> Self {
        let field = |name: &str| {
            identity.and_then(|report| {
                report
                    .entries
                    .iter()
                    .find(|entry| entry.name.as_deref() == Some(name))
                    .map(|entry| entry.value.clone())
            })
        };
        Self {
            serial: serial.to_owned(),
            product: driver.product_name(field("device_type").as_deref()),
            model: field("model_id"),
            firmware: field("sw_version"),
            hardware: field("hw_version"),
        }
    }

    /// The `device` block of a discovery message.
    fn json(&self) -> Value {
        let mut device = Map::new();
        device.insert("identifiers".to_owned(), json!([self.serial]));
        // Named for the product where one is known, and for the vendor where it is not — a device page
        // should not claim to be a product it is not.
        let name = match self.product {
            Some(product) => format!("{product} {}", self.serial),
            None => format!("Growatt {}", self.serial),
        };
        device.insert("name".to_owned(), json!(name));
        device.insert("manufacturer".to_owned(), json!("Growatt"));
        device.insert("serial_number".to_owned(), json!(self.serial));

        // `model` is what a person reads and `model_id` is the manufacturer's code for it. The datalogger
        // reports its own model as a code — `GTSW0000` — which Home Assistant would otherwise render as
        // the whole identity of the thing, "GTSW0000 by Growatt". Both are kept: the product names what
        // the device is, and the code stays available as the manufacturer wrote it.
        if let Some(product) = self.product {
            device.insert("model".to_owned(), json!(product));
            if let Some(code) = self.model.as_ref() {
                device.insert("model_id".to_owned(), json!(code));
            }
        } else if let Some(code) = self.model.as_ref() {
            // Nothing better to say than the code, which beats saying nothing.
            device.insert("model".to_owned(), json!(code));
        }

        for (key, value) in [
            ("sw_version", self.firmware.as_ref()),
            ("hw_version", self.hardware.as_ref()),
        ] {
            if let Some(value) = value {
                device.insert(key.to_owned(), json!(value));
            }
        }
        Value::Object(device)
    }
}

/// The discovery message for one entity.
#[derive(Debug, Clone, Copy)]
pub struct Discovery<'a> {
    /// What is being announced.
    pub entity: &'a Entity,
    /// Where it lives on the broker.
    pub topics: &'a Topics,
    /// Which device it belongs to.
    pub device: &'a DeviceBlock,
}

impl Discovery<'_> {
    /// The retained message that announces it.
    pub fn publication(&self) -> Publication {
        Publication::retained(
            self.topics
                .discovery(self.entity.component, &self.device.serial, self.entity.key),
            self.payload(),
        )
    }

    /// The message body.
    fn payload(&self) -> Vec<u8> {
        let mut config = self.common();
        self.describe_state(&mut config);
        self.describe_commands(&mut config);
        // Serialising a `Map` cannot fail, so the fallback is unreachable rather than a decision: an empty
        // object would leave Home Assistant with a broken entity, which is why it is not a silent default.
        serde_json::to_vec(&Value::Object(config)).unwrap_or_else(|error| {
            tracing::error!(%error, key = self.entity.key, "could not serialise a discovery message");
            Vec::new()
        })
    }

    /// The fields every component has.
    fn common(&self) -> Map<String, Value> {
        let entity = self.entity;
        let device = &self.device.serial;
        let mut config = Map::new();

        config.insert("name".to_owned(), json!(entity.name));
        config.insert("unique_id".to_owned(), json!(self.topics.unique_id(device, entity.key)));
        config.insert("object_id".to_owned(), json!(self.topics.unique_id(device, entity.key)));
        // A control with no source is optimistic: nothing reports its value back, so Home Assistant shows
        // what it last set. Announcing a state topic nothing publishes to would leave it unknown forever.
        if let Some(topic) = self.topics.topic_for(entity, device) {
            config.insert("state_topic".to_owned(), json!(topic));
            config.insert("value_template".to_owned(), json!(self.value_template()));
        } else {
            config.insert("optimistic".to_owned(), json!(true));
        }

        // Both signals, so an entity is available only while this program is running and the device is
        // reporting — except for the two that exist to say the device is not.
        config.insert(
            "availability".to_owned(),
            Value::Array(
                self.topics
                    .availability(entity, device)
                    .into_iter()
                    .map(|topic| json!({ "topic": topic }))
                    .collect(),
            ),
        );
        config.insert("availability_mode".to_owned(), json!("all"));
        config.insert("payload_available".to_owned(), json!(String::from_utf8_lossy(ONLINE)));
        config.insert(
            "payload_not_available".to_owned(),
            json!(String::from_utf8_lossy(OFFLINE)),
        );

        config.insert("device".to_owned(), self.device.json());
        config.insert(
            "origin".to_owned(),
            json!({
                "name": env!("CARGO_PKG_NAME"),
                "sw_version": crate::VERSION,
                "support_url": env!("CARGO_PKG_REPOSITORY"),
            }),
        );

        if let Some(category) = entity.category {
            config.insert("entity_category".to_owned(), json!(category.as_str()));
        }
        if let Some(unit) = entity.unit {
            config.insert("unit_of_measurement".to_owned(), json!(unit));
        }
        if let Some(class) = entity.device_class {
            config.insert("device_class".to_owned(), json!(class));
        }
        if let Some(decimals) = entity.precision {
            config.insert("suggested_display_precision".to_owned(), json!(decimals));
        }
        config
    }

    /// How Home Assistant picks this entity's field out of the shared object.
    ///
    /// `default('')` covers the field being absent, which leaves the last reading in place rather than
    /// replacing it with `unknown` — though Home Assistant logs it rather than ignoring it, which is why a
    /// field that is regularly absent is published alone instead. See the module note.
    ///
    /// A flags word is rendered here rather than on the topic: the state topic keeps the raw number, which
    /// is what another subscriber wants, and the hexadecimal is presentation.
    fn value_template(&self) -> String {
        let key = self.entity.key;
        if self.entity.shape == Shape::Flags {
            return format!(
                "{{% set word = value_json.{key} | default(none) %}}\
                 {{% if word is not none %}}0x{{{{ '%04X' | format(word) }}}}{{% endif %}}"
            );
        }
        format!("{{{{ value_json.{key} | default('') }}}}")
    }

    /// The fields that describe how the state reads.
    fn describe_state(&self, config: &mut Map<String, Value>) {
        match self.entity.shape {
            Shape::Reading(state_class) => {
                if let Some(class) = state_class {
                    config.insert("state_class".to_owned(), json!(class.as_str()));
                }
            }
            // Nothing to add: a flags word carries no unit, no device class and no state class, and its
            // hexadecimal rendering is in the value template.
            Shape::Flags => {}
            Shape::Signal { on, off } => {
                config.insert("payload_on".to_owned(), json!(on));
                config.insert("payload_off".to_owned(), json!(off));
            }
            Shape::Numeric(Bounds { min, max }) => {
                config.insert("min".to_owned(), json!(min));
                config.insert("max".to_owned(), json!(max));
                config.insert("step".to_owned(), json!(1));
                // A slider hides the number it is setting, and these are watts and percentages someone
                // wants to read as well as change.
                config.insert("mode".to_owned(), json!("box"));
            }
            Shape::Toggle => {
                // The device stores a flag as 0 or 1, and that is what the settings object carries.
                config.insert("state_on".to_owned(), json!(1));
                config.insert("state_off".to_owned(), json!(0));
            }
            Shape::Choice(labels) => {
                config.insert("options".to_owned(), json!(labels));
            }
            Shape::TimeOfDay => {
                config.insert("pattern".to_owned(), json!(TIME_PATTERN));
                config.insert("min".to_owned(), json!(5));
                config.insert("max".to_owned(), json!(5));
            }
            // A button has no state at all, so it must not claim a state topic: Home Assistant would show
            // it as unavailable until something published one.
            Shape::Action => {
                config.remove("state_topic");
                config.remove("value_template");
            }
        }
    }

    /// The fields that let Home Assistant change it.
    ///
    /// `optimistic: false` with a state topic is the pairing that matters: an MQTT switch with no state
    /// topic goes optimistic and will cheerfully display a value the device refused. Every write here is
    /// confirmed by a read-back before anything is republished.
    fn describe_commands(&self, config: &mut Map<String, Value>) {
        if !self.entity.is_writable() {
            return;
        }
        let key = self.entity.key;
        // A control with a read-back must not go optimistic; one without has nothing else it could be, and
        // `common` has already said so. Deciding it once here keeps the two from contradicting each other.
        let confirmed = self.entity.source.is_some();
        config.insert(
            "command_topic".to_owned(),
            json!(self.topics.command(&self.device.serial)),
        );

        match self.entity.shape {
            // A switch sends whole payloads rather than a template, so the command object is written out.
            Shape::Toggle => {
                config.insert("payload_on".to_owned(), json!(json!({ key: 1 }).to_string()));
                config.insert("payload_off".to_owned(), json!(json!({ key: 0 }).to_string()));
                if confirmed {
                    config.insert("optimistic".to_owned(), json!(false));
                }
            }
            Shape::Numeric(_) => {
                config.insert(
                    "command_template".to_owned(),
                    json!(format!(r#"{{"{key}": {{{{ value }}}}}}"#)),
                );
                if confirmed {
                    config.insert("optimistic".to_owned(), json!(false));
                }
            }
            Shape::Choice(_) | Shape::TimeOfDay => {
                config.insert(
                    "command_template".to_owned(),
                    json!(format!(r#"{{"{key}": "{{{{ value }}}}"}}"#)),
                );
                if confirmed {
                    config.insert("optimistic".to_owned(), json!(false));
                }
            }
            Shape::Action => {
                config.insert("payload_press".to_owned(), json!(json!({ key: 1 }).to_string()));
                // Rebooting hardware is not something to offer by accident.
                config.insert("enabled_by_default".to_owned(), json!(false));
            }
            Shape::Reading(_) | Shape::Signal { .. } | Shape::Flags => {}
        }
    }
}

/// Whether a component is one Home Assistant lets a user act on.
///
/// Used only to check the catalogue against itself: a writable shape published as a `sensor` would offer
/// no control, and a read-only shape published as a `switch` would offer one that goes nowhere.
pub const fn is_control(component: Component) -> bool {
    matches!(
        component,
        Component::Number | Component::Switch | Component::Select | Component::Text | Component::Button
    )
}

#[cfg(test)]
mod tests {
    use super::{DeviceBlock, Discovery, TIME_PATTERN, is_control};
    use crate::control::{ConfigView, IdentityView};
    use crate::growatt::driver::Growatt;
    use crate::homeassistant::command::Permitted;
    use crate::homeassistant::entity::{Catalogue, Component, Entity, Presence};
    use crate::homeassistant::topics::Topics;
    use serde_json::Value;

    /// The device as it looks before its identity report has arrived.
    fn bare() -> DeviceBlock {
        DeviceBlock::new("0EXAMPLE00000001", None, &Growatt)
    }

    /// An identity report carrying the fields named, as the device sends them: numbers as text.
    fn report(fields: &[(&'static str, &str)]) -> IdentityView {
        IdentityView {
            declared: fields.len().try_into().expect("a test report is a handful of fields"),
            truncated: false,
            endpoint: None,
            entries: fields
                .iter()
                .map(|(name, value)| ConfigView {
                    register: 0,
                    name: Some((*name).to_owned()),
                    role: None,
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    /// The `name` of a device block for a device reporting this type code.
    fn device_name(device_type: &str) -> String {
        let report = report(&[("device_type", device_type)]);
        let json = DeviceBlock::new("0EXAMPLE00000001", Some(&report), &Growatt).json();
        json.get("name")
            .and_then(Value::as_str)
            .expect("a device block always names the device")
            .to_owned()
    }

    #[test]
    fn the_product_comes_from_the_reported_device_type() {
        // The device names its own product in config key 13, so nothing here depends on the serial.
        let report = report(&[("device_type", "72"), ("model_id", "GTSW0000")]);
        let block = DeviceBlock::new("0EXAMPLE00000001", Some(&report), &Growatt);
        assert_eq!(block.product, Some("NEXA 2000"));
    }

    #[test]
    fn the_product_name_is_the_model_and_the_device_s_code_is_the_model_id() {
        // Home Assistant renders the device as "<model> by <manufacturer>", so putting the vendor's code
        // in `model` shows the page as "GTSW0000 by Growatt" — true, and useless to read. The code keeps
        // its own field, which is what `model_id` is for.
        let report = report(&[("device_type", "72"), ("model_id", "GTSW0000")]);
        let json = DeviceBlock::new("0EXAMPLE00000001", Some(&report), &Growatt).json();
        assert_eq!(json["model"], "NEXA 2000");
        assert_eq!(json["model_id"], "GTSW0000");
    }

    #[test]
    fn an_unknown_product_falls_back_to_the_code_rather_than_showing_nothing() {
        // Nothing better to say. The code at least distinguishes two unrecognised products from each
        // other, where an absent model says only that this program has not been taught the type code.
        let report = report(&[("device_type", "99"), ("model_id", "GTSW0000")]);
        let json = DeviceBlock::new("0EXAMPLE00000001", Some(&report), &Growatt).json();
        assert_eq!(json["model"], "GTSW0000");
        assert!(json.get("model_id").is_none(), "the code is not repeated");
    }

    #[test]
    fn a_device_with_no_identity_report_yet_claims_no_product() {
        // The type code arrives with the report, about five seconds into a session. Until then the page is
        // announced under the vendor name and re-announced when the product is known — rather than
        // guessing a product from the serial, which no vendor source maps.
        let json = DeviceBlock::new("0EXAMPLE00000001", None, &Growatt).json();
        assert_eq!(json["name"], "Growatt 0EXAMPLE00000001");
        assert!(json.get("model").is_none());
        assert!(json.get("model_id").is_none());
    }

    #[test]
    fn the_device_page_is_named_for_the_product_the_device_reports() {
        assert_eq!(device_name("72"), "NEXA 2000 0EXAMPLE00000001");
        assert_eq!(device_name("61"), "NOAH 2000 0EXAMPLE00000001");
    }

    #[test]
    fn an_unrecognised_type_code_is_named_for_the_vendor_rather_than_guessed() {
        // Not refused and not labelled as any product: a code this build has not been taught is still a
        // device worth serving, and claiming a model would be worse than naming none.
        assert_eq!(device_name("99"), "Growatt 0EXAMPLE00000001");
    }

    /// One entity's discovery payload, parsed back.
    fn payload(entity: &Entity) -> Value {
        let topics = Topics {
            instance: "attic".to_owned(),
            ..Topics::default()
        };
        let device = bare();
        let discovery = Discovery {
            entity,
            topics: &topics,
            device: &device,
        };
        serde_json::from_slice(&discovery.publication().payload).expect("valid JSON")
    }

    /// One entity from the default catalogue, by key.
    fn entity(key: &str) -> Entity {
        Catalogue::default()
            .entities(&Growatt)
            .into_iter()
            .find(|entity| entity.key == key)
            .unwrap_or_else(|| panic!("no entity {key}"))
    }

    #[test]
    fn a_reading_names_its_field_its_unit_and_both_availability_topics() {
        let config = payload(&entity("ac_power"));
        assert_eq!(config["state_topic"], "heliobridge/0EXAMPLE00000001/state");
        assert_eq!(config["value_template"], "{{ value_json.ac_power | default('') }}");
        assert_eq!(config["unit_of_measurement"], "W");
        assert_eq!(config["device_class"], "power");
        assert_eq!(config["state_class"], "measurement");
        assert_eq!(config["availability_mode"], "all");
        assert_eq!(
            config["availability"],
            serde_json::json!([
                { "topic": "heliobridge/bridge/attic/availability" },
                { "topic": "heliobridge/0EXAMPLE00000001/availability" },
            ])
        );
        assert_eq!(config["payload_available"], "online");
        // A reading offers no control.
        assert!(config.get("command_topic").is_none());
    }

    #[test]
    fn the_entities_that_report_an_outage_list_only_the_bridge() {
        // If either listed the device's availability it would go unavailable exactly when it became worth
        // reading, which is the failure this whole distinction exists to avoid.
        for key in ["connected", "last_update", "bridge_version"] {
            let entity = entity(key);
            assert_eq!(entity.presence, Presence::Bridge, "{key}");
            let config = payload(&entity);
            assert_eq!(
                config["availability"],
                serde_json::json!([{ "topic": "heliobridge/bridge/attic/availability" }]),
                "{key}"
            );
            // The stamp is on a sub-topic of its own, because it does not exist until a frame arrives.
            let expected = if key == "last_update" {
                "heliobridge/0EXAMPLE00000001/status/last_update"
            } else {
                "heliobridge/0EXAMPLE00000001/status"
            };
            assert_eq!(config["state_topic"], expected, "{key}");
        }

        // Presence and category are separate judgements. These two qualify every reading on the page — how
        // current is it, and is there a device behind it — so they are read beside those readings rather
        // than in a collapsed block.
        for key in ["connected", "last_update"] {
            assert!(payload(&entity(key)).get("entity_category").is_none(), "{key}");
        }
        // Which build did the decoding is about the software, so it belongs with the other versions.
        assert_eq!(payload(&entity("bridge_version"))["entity_category"], "diagnostic");
    }

    #[test]
    fn a_flags_word_renders_as_hexadecimal_and_claims_no_quantity() {
        // `Grid faults: 0` reads as a count of faults, and the specification's bit tables are indexed by
        // hex, so `0x0400` can be looked up where `1024` has to be converted first.
        let config = payload(&entity("grid_faults"));
        let template = config["value_template"].as_str().expect("a template");
        assert!(template.contains("value_json.grid_faults"), "{template}");
        assert!(template.contains("0x"), "{template}");
        assert!(template.contains("'%04X'"), "{template}");
        // Absent renders empty, which Home Assistant reads as no update rather than as a value.
        assert!(template.contains("default(none)"), "{template}");

        for absent in [
            "state_class",
            "unit_of_measurement",
            "device_class",
            "suggested_display_precision",
        ] {
            assert!(config.get(absent).is_none(), "a bitfield must not carry {absent}");
        }
        assert_eq!(config["entity_category"], "diagnostic");
    }

    #[test]
    fn the_raw_word_stays_on_the_state_topic() {
        // Hexadecimal is presentation. Another subscriber wants the number.
        let payload = crate::homeassistant::state::StatePayload::telemetry(
            &crate::control::TelemetryView {
                timestamp: None,
                readings: vec![crate::control::ReadingView {
                    register: 3,
                    name: "grid_faults",
                    raw: 1024,
                    value: "1024".to_owned(),
                    unit: "",
                    confidence: "observed",
                }],
            },
            &crate::homeassistant::state::Fields::of(&Catalogue::default().entities(&Growatt)),
        );
        assert_eq!(payload.get("grid_faults"), Some(&serde_json::json!(1024)));
    }

    #[test]
    fn a_timestamp_carries_no_state_class() {
        // Home Assistant rejects one on a timestamp.
        let config = payload(&entity("last_update"));
        assert_eq!(config["device_class"], "timestamp");
        assert!(config.get("state_class").is_none());
        assert!(config.get("unit_of_measurement").is_none());
    }

    #[test]
    fn a_connectivity_sensor_says_which_payload_means_which() {
        let config = payload(&entity("connected"));
        assert_eq!(config["device_class"], "connectivity");
        assert_eq!(config["payload_on"], "online");
        assert_eq!(config["payload_off"], "offline");
    }

    #[test]
    fn a_number_carries_the_registers_own_bounds_and_a_command_template() {
        let config = payload(&entity("charge_limit_upper"));
        assert_eq!(config["min"], 70);
        assert_eq!(config["max"], 100);
        assert_eq!(config["step"], 1);
        assert_eq!(config["command_topic"], "heliobridge/0EXAMPLE00000001/set");
        assert_eq!(config["command_template"], r#"{"charge_limit_upper": {{ value }}}"#);
        // Telemetry, not settings: this is one of the two fields the device also reports in every frame.
        assert_eq!(config["state_topic"], "heliobridge/0EXAMPLE00000001/state");
        assert_eq!(config["optimistic"], false);
        assert_eq!(config["entity_category"], "config");
    }

    #[test]
    fn a_switch_sends_whole_payloads_and_reads_the_flag_back() {
        // The command payloads have to be the JSON the command handler parses, and the states have to be
        // what the settings object actually carries — 0 and 1, as the device stores them.
        let config = payload(&entity("grid_power_allowed"));
        assert_eq!(config["payload_on"], r#"{"grid_power_allowed":1}"#);
        assert_eq!(config["payload_off"], r#"{"grid_power_allowed":0}"#);
        assert_eq!(config["state_on"], 1);
        assert_eq!(config["state_off"], 0);
        assert_eq!(config["optimistic"], false);
    }

    #[test]
    fn a_work_mode_offers_the_labels_the_device_uses() {
        let config = payload(&entity("slot1_work_mode"));
        assert_eq!(
            config["options"],
            serde_json::json!(["load_first", "battery_first", "smart_self_use"])
        );
        assert_eq!(config["command_template"], r#"{"slot1_work_mode": "{{ value }}"}"#);
    }

    #[test]
    fn a_slot_boundary_is_a_text_entity_with_a_pattern() {
        // Home Assistant has no MQTT `time` component, so the pattern is what keeps a nonsense value out.
        let config = payload(&entity("slot1_start_time"));
        assert_eq!(config["pattern"], TIME_PATTERN);
        assert_eq!(config["min"], 5);
        assert_eq!(config["max"], 5);
        assert_eq!(config["command_template"], r#"{"slot1_start_time": "{{ value }}"}"#);
    }

    #[test]
    fn the_pattern_accepts_a_time_and_refuses_what_is_not_one() {
        // Checked here rather than trusted, since the only thing that reads it is Home Assistant.
        let matches = |value: &str| {
            let (hours, minutes) = value.split_once(':').expect("a colon");
            hours.len() == 2 && minutes.len() == 2 && {
                let hour: u32 = hours.parse().unwrap_or(99);
                let minute: u32 = minutes.parse().unwrap_or(99);
                hour < 24 && minute < 60
            }
        };
        assert!(TIME_PATTERN.starts_with('^') && TIME_PATTERN.ends_with('$'));
        for value in ["00:00", "23:59", "07:05"] {
            assert!(matches(value), "{value}");
        }
        for value in ["24:00", "07:60"] {
            assert!(!matches(value), "{value}");
        }
    }

    #[test]
    fn identity_fills_the_device_block_and_its_absence_leaves_it_out() {
        let bare = bare();
        let config = payload(&entity("ac_power"));
        assert_eq!(config["device"]["identifiers"], serde_json::json!(["0EXAMPLE00000001"]));
        assert_eq!(config["device"]["manufacturer"], "Growatt");
        assert_eq!(config["device"]["serial_number"], "0EXAMPLE00000001");
        assert!(config["device"].get("model").is_none(), "nothing to say yet");
        assert_eq!(bare.model, None);
        assert_eq!(config["origin"]["name"], "heliobridge");
    }

    #[test]
    fn a_validated_entity_is_announced_on_a_topic_of_its_own() {
        // Everything else reads a field out of the shared settings object and tolerates one that has not
        // arrived. A `text` entity validates its value instead of ignoring an empty one, so it must not be
        // pointed at an object that is partial while a resync runs.
        let catalogue = Catalogue::default().entities(&Growatt);
        let slot_time = catalogue
            .iter()
            .find(|entity| entity.key == "slot1_start_time")
            .expect("slot 1 is exposed");
        let slot_power = catalogue
            .iter()
            .find(|entity| entity.key == "slot1_output_power")
            .expect("slot 1 is exposed");

        assert!(slot_time.published_alone());
        assert!(!slot_power.published_alone());
        assert_eq!(
            payload(slot_time)["state_topic"],
            "heliobridge/0EXAMPLE00000001/settings/slot1_start_time"
        );
        assert_eq!(
            payload(slot_power)["state_topic"],
            "heliobridge/0EXAMPLE00000001/settings"
        );
    }

    #[test]
    fn every_entity_produces_a_message_home_assistant_can_read() {
        // The whole catalogue, so a register added to the map cannot produce an entity that fails to
        // announce itself.
        for entity in Catalogue::default().entities(&Growatt) {
            let config = payload(&entity);
            let object = config.as_object().expect("an object");
            assert!(object.contains_key("unique_id"), "{}", entity.key);
            assert!(object.contains_key("name"), "{}", entity.key);
            assert!(object.contains_key("availability"), "{}", entity.key);
            // A state topic exactly when something publishes one: an action has no state to report, and
            // an optimistic control has no read-back to report it from.
            assert_eq!(
                object.contains_key("state_topic"),
                entity.source.is_some() && !matches!(entity.shape, crate::homeassistant::entity::Shape::Action),
                "{}",
                entity.key
            );
            // `optimistic: false` is set for every control that *has* a read-back, so the presence of the
            // key says nothing; only its value distinguishes the two.
            assert_eq!(
                object.get("optimistic") == Some(&serde_json::json!(true)),
                entity.source.is_none(),
                "{}",
                entity.key
            );
            assert_eq!(
                object.contains_key("command_topic"),
                is_control(entity.component),
                "{} is a {} but its command topic disagrees",
                entity.key,
                entity.component
            );
        }
    }

    #[test]
    fn two_entities_never_claim_one_identifier() {
        // A duplicate `unique_id` makes Home Assistant drop the second entity silently.
        let entities = Catalogue {
            slots: 9,
            ..Catalogue::default()
        }
        .entities(&Growatt);
        let mut seen: Vec<&str> = entities.iter().map(|entity| entity.key).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "the catalogue repeats a key");
        assert!(total > 40, "only {total} entities");
    }

    #[test]
    fn refusing_writes_leaves_readings_rather_than_controls() {
        // What HELIOBRIDGE_ALLOW_WRITES=false produces: still worth seeing, nothing to touch.
        for entity in (Catalogue {
            permitted: Permitted {
                writes: false,
                ..Permitted::default()
            },
            ..Catalogue::default()
        })
        .entities(&Growatt)
        {
            let config = payload(&entity);
            assert!(
                config.get("command_topic").is_none(),
                "{} still offers a command topic",
                entity.key
            );
            assert!(!is_control(entity.component), "{} is still a control", entity.key);
        }
    }

    #[test]
    fn one_setting_can_be_left_as_a_reading_while_the_rest_stay_writable() {
        // The control is not offered, and the command topic it would have written to is not named — so
        // Home Assistant has nothing to send even before the command handler refuses it.
        let catalogue = Catalogue {
            permitted: Permitted {
                power_plus: false,
                ..Permitted::default()
            },
            ..Catalogue::default()
        };
        let entities = catalogue.entities(&Growatt);

        let power_plus = entities
            .iter()
            .find(|entity| entity.key == "power_plus")
            .expect("still published");
        assert_eq!(power_plus.component, Component::Sensor);
        assert!(payload(power_plus).get("command_topic").is_none());
        // Visible, so whether it is on can still be read.
        assert_eq!(
            payload(power_plus)["state_topic"],
            "heliobridge/0EXAMPLE00000001/settings"
        );

        // Every other switch is untouched.
        let always_on = entities
            .iter()
            .find(|entity| entity.key == "always_on")
            .expect("published");
        assert_eq!(always_on.component, Component::Switch);
        assert!(payload(always_on).get("command_topic").is_some());
    }
}
