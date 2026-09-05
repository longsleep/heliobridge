//! The JSON objects the state topics carry.
//!
//! One object per topic rather than one topic per field: a telemetry frame carries over a hundred
//! registers, and publishing each separately would be a hundred writes every five seconds instead of one.
//! Each entity's `value_template` picks its own field out.
//!
//! # An object carries exactly the fields the entities read
//!
//! Built against the same catalogue the discovery messages come from, so there is no field nothing reads
//! and no entity reading a field that is never published. That matters in both directions: an
//! uninterpretable register would be noise on the broker, and a per-pack reading for a pack that is not
//! attached would decode to absolute zero.

use std::collections::HashSet;

use serde_json::{Map, Number, Value, json};

use crate::control::{ConfigView, SettingView, TelemetryView};
use crate::homeassistant::broker::Publication;
use crate::homeassistant::entity::Entity;
use crate::homeassistant::entity::{FIRMWARE_VERSION, LAST_UPDATE};
use crate::homeassistant::topics::{OFFLINE, ONLINE};

/// Which fields a device publishes.
#[derive(Debug, Clone, Default)]
pub struct Fields(HashSet<&'static str>);

impl Fields {
    /// The keys of a catalogue of entities.
    pub fn of(entities: &[Entity]) -> Self {
        Self(entities.iter().map(|entity| entity.key).collect())
    }

    /// Whether a field has an entity reading it.
    pub fn contains(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    /// How many fields there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One state object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatePayload(Map<String, Value>);

impl StatePayload {
    /// The readings of one telemetry frame.
    pub fn telemetry(view: &TelemetryView, fields: &Fields) -> Self {
        Self(
            view.readings
                .iter()
                .filter(|reading| fields.contains(reading.name))
                .map(|reading| (reading.name.to_owned(), quantity(&reading.value)))
                .collect(),
        )
    }

    /// The settings a session has read back.
    pub fn settings(views: &[SettingView], fields: &Fields) -> Self {
        Self(
            views
                .iter()
                .filter(|setting| fields.contains(setting.name))
                .map(|setting| (setting.name.to_owned(), quantity(&setting.value)))
                .collect(),
        )
    }

    /// What this bridge knows about the device rather than from it, plus which build knows it.
    ///
    /// `last_update` is left out entirely until a frame has arrived, rather than being sent as null or as a
    /// zero time: an absent field renders empty, which Home Assistant treats as no update, so the sensor
    /// stays unknown instead of claiming the device last reported at the epoch.
    pub fn status(connected: bool) -> Self {
        let mut object = Map::new();
        object.insert(
            "connected".to_owned(),
            json!(String::from_utf8_lossy(if connected { ONLINE } else { OFFLINE })),
        );
        // A constant for the life of the process, so it rides along with the topic that is published
        // whenever this bridge's view of the device changes rather than needing one of its own.
        object.insert("bridge_version".to_owned(), json!(crate::VERSION));
        Self(object)
    }

    /// The assembled firmware version, for the sub-topic that carries it alone.
    pub fn firmware_version(version: &str) -> Self {
        let mut object = Map::new();
        object.insert(FIRMWARE_VERSION.to_owned(), json!(version));
        Self(object)
    }

    /// When the last telemetry frame arrived, for the sub-topic that carries it alone.
    ///
    /// Its own topic because it does not exist until a frame has arrived, and an absent field templates to
    /// an empty state — which Home Assistant logs rather than ignores.
    pub fn last_update(stamp: &str) -> Self {
        let mut object = Map::new();
        object.insert(LAST_UPDATE.to_owned(), json!(stamp));
        Self(object)
    }

    /// The datalogger's configuration, from the identity report.
    ///
    /// Only the named registers an entity was declared for. The report carries 32 and most are static,
    /// inert or identifying; publishing the rest would put a device page's worth of constants on a
    /// dashboard.
    pub fn config(entries: &[ConfigView], fields: &Fields) -> Self {
        let mut object = Map::new();
        for entry in entries {
            let Some(name) = entry.name.as_deref() else { continue };
            if !fields.contains(name) {
                continue;
            }
            // A config value arrives as octets whatever the field means — usually text, though some
            // registers carry NUL padding or raw bytes — so a numeric entity needs it parsed. A value that
            // will not parse is left out rather than sent as text: Home Assistant would take "abc" for a
            // dBm reading and show it as unknown anyway, but noisily.
            let value = match name {
                "wifi_signal" | "data_interval" => match entry.value.trim().parse::<i64>() {
                    Ok(number) => json!(number),
                    Err(_) => continue,
                },
                _ => json!(printable(&entry.value)),
            };
            object.insert(name.to_owned(), value);
        }
        Self(object)
    }

    /// Whether there is anything to publish.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many fields it carries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// One field, for a test or a log line.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// The message body.
    pub fn into_bytes(self) -> Vec<u8> {
        // Serialising a map of numbers and strings cannot fail.
        serde_json::to_vec(&Value::Object(self.0)).unwrap_or_else(|error| {
            tracing::error!(%error, "could not serialise a state object");
            Vec::new()
        })
    }

    /// A transient publication on the given topic.
    ///
    /// Not retained: it is replaced within seconds and a retained copy would be read as current long after
    /// it stopped being true. Availability carries the liveness signal instead.
    pub fn publication(self, topic: impl Into<String>) -> Publication {
        Publication::state(topic, self.into_bytes())
    }

    /// The same, kept by the broker as the topic's last known value.
    ///
    /// Only for status, which is the one object that must survive a subscriber arriving late: it is
    /// published when something changes rather than on a cycle, and while the device is offline nothing
    /// will republish it.
    pub fn retained(self, topic: impl Into<String>) -> Publication {
        Publication::retained(topic, self.into_bytes())
    }
}

/// One config value, fit to be a Home Assistant state.
///
/// Two things the device does that a state cannot carry. Values arrive **NUL-padded** — a fixed-width
/// buffer sent as it sits in memory — and a NUL inside a JSON string is legal but renders as nothing
/// anybody can see, so a reader cannot tell where the value stopped. And Home Assistant caps a state at
/// 255 characters, silently: a longer one is rejected and the entity keeps its previous value, which reads
/// as a device that stopped reporting.
///
/// So trailing padding and control octets go, and what is left is truncated with an ellipsis that says so.
/// Anything needing the exact octets has the control API, which returns the value as sent.
fn printable(value: &str) -> String {
    let cleaned: String = value
        .trim_end_matches(|c: char| c == '\0' || c.is_whitespace())
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    if cleaned.chars().count() <= STATE_LIMIT {
        return cleaned;
    }
    cleaned.chars().take(STATE_LIMIT - 1).chain(['…']).collect()
}

/// What Home Assistant will accept as a state, in characters.
const STATE_LIMIT: usize = 255;

/// A rendered value as JSON: a number where it is one, a string where it is not.
///
/// The rendering comes from the decoder, whose scalings are all powers of ten down to thousandths, so
/// parsing it back is exact rather than approximate. What does not parse is a label or a time of day, and
/// those belong in the object as the text an entity's options or pattern expect.
fn quantity(rendered: &str) -> Value {
    if let Ok(whole) = rendered.parse::<i64>() {
        return json!(whole);
    }
    match rendered.parse::<f64>().ok().and_then(Number::from_f64) {
        Some(number) => Value::Number(number),
        None => json!(rendered),
    }
}

#[cfg(test)]
mod tests {
    use super::{Fields, StatePayload, quantity};
    use crate::control::{ReadingView, SettingView, TelemetryView};
    use crate::growatt::driver::Growatt;
    use crate::homeassistant::entity::Catalogue;
    use crate::homeassistant::entity::LAST_UPDATE;
    use serde_json::json;

    fn reading(name: &'static str, value: &str) -> ReadingView {
        ReadingView {
            register: 5,
            name,
            raw: 0,
            value: value.to_owned(),
            unit: "W",
            confidence: "verified",
        }
    }

    fn fields() -> Fields {
        Fields::of(&Catalogue::default().entities(&Growatt))
    }

    #[test]
    fn a_number_stays_a_number_and_a_label_stays_a_label() {
        // Both matter: a numeric sensor is fed the number, and a select's state has to match one of its
        // options exactly.
        assert_eq!(quantity("-100"), json!(-100));
        assert_eq!(quantity("99.9"), json!(99.9));
        assert_eq!(quantity("0"), json!(0));
        assert_eq!(quantity("smart_self_use"), json!("smart_self_use"));
        assert_eq!(quantity("23:59"), json!("23:59"));
        // Nothing in the decoder produces these, but a non-finite float has no JSON form, so it must
        // survive as text rather than as a malformed document.
        assert_eq!(quantity("NaN"), json!("NaN"));
        assert_eq!(quantity("inf"), json!("inf"));
    }

    #[test]
    fn a_state_object_carries_only_fields_an_entity_reads() {
        // The invariant that keeps the two halves in step: no orphan field, no entity reading nothing.
        let view = TelemetryView {
            timestamp: None,
            readings: vec![
                reading("ac_power", "-100"),
                reading("unknown_81", "42"),
                reading("battery_soc_total", "88"),
            ],
        };
        let payload = StatePayload::telemetry(&view, &fields());
        assert_eq!(payload.get("ac_power"), Some(&json!(-100)));
        assert_eq!(payload.get("battery_soc_total"), Some(&json!(88)));
        assert_eq!(payload.get("unknown_81"), None, "nothing can interpret it");
        assert_eq!(payload.len(), 2);
    }

    #[test]
    fn an_unattached_pack_is_left_out_rather_than_reported_at_absolute_zero() {
        let one_pack = Fields::of(
            &Catalogue {
                packs: 1,
                ..Catalogue::default()
            }
            .entities(&Growatt),
        );
        let view = TelemetryView {
            timestamp: None,
            readings: vec![reading("battery1_temp", "30.4"), reading("battery2_temp", "-273.1")],
        };
        let payload = StatePayload::telemetry(&view, &one_pack);
        assert_eq!(payload.get("battery1_temp"), Some(&json!(30.4)));
        assert_eq!(payload.get("battery2_temp"), None);
    }

    #[test]
    fn settings_render_as_the_entities_expect_to_read_them() {
        // A switch reads 0 or 1, a select reads its label, a text entity reads HH:MM.
        let views = vec![
            SettingView {
                register: 326,
                name: "grid_power_allowed",
                raw: 1,
                value: "1".to_owned(),
                unit: "",
            },
            SettingView {
                register: 256,
                name: "slot1_work_mode",
                raw: 2,
                value: "smart_self_use".to_owned(),
                unit: "",
            },
            SettingView {
                register: 254,
                name: "slot1_start_time",
                raw: 0,
                value: "00:00".to_owned(),
                unit: "",
            },
        ];
        let payload = StatePayload::settings(&views, &fields());
        assert_eq!(payload.get("grid_power_allowed"), Some(&json!(1)));
        assert_eq!(payload.get("slot1_work_mode"), Some(&json!("smart_self_use")));
        assert_eq!(payload.get("slot1_start_time"), Some(&json!("00:00")));
    }

    #[test]
    fn a_slot_beyond_the_configured_count_is_not_published() {
        // The session reads back only the exposed slots, so publishing a ninth would be a field with no
        // entity and no value behind it.
        let views = vec![SettingView {
            register: 294,
            name: "slot9_start_time",
            raw: 0,
            value: "00:00".to_owned(),
            unit: "",
        }];
        assert!(StatePayload::settings(&views, &fields()).is_empty());
    }

    #[test]
    fn status_says_nothing_about_a_frame_that_never_arrived() {
        // Not even as an absent field. The stamp goes on a sub-topic of its own, so before the first frame
        // nothing is published for it at all — where an absent field templates to an empty state, which
        // Home Assistant logs as invalid rather than ignoring.
        for connected in [true, false] {
            assert_eq!(StatePayload::status(connected).get(LAST_UPDATE), None);
        }
        assert_eq!(StatePayload::status(false).get("connected"), Some(&json!("offline")));
        assert_eq!(StatePayload::status(true).get("connected"), Some(&json!("online")));
    }

    #[test]
    fn the_last_update_stamp_is_its_own_object() {
        let payload = StatePayload::last_update("2026-08-09T12:00:00+02:00");
        assert_eq!(payload.get(LAST_UPDATE), Some(&json!("2026-08-09T12:00:00+02:00")));
        // Nothing else, so it cannot overwrite the shared status object's fields on its own topic.
        assert_eq!(payload.get("connected"), None);
        assert_eq!(payload.get("bridge_version"), None);
    }

    #[test]
    fn status_says_which_build_produced_the_readings() {
        // Every field on the device page is this program's interpretation of a register, so which build did
        // the interpreting belongs there — and it has to be true whether or not the device is present.
        for connected in [true, false] {
            assert_eq!(
                StatePayload::status(connected).get("bridge_version"),
                Some(&json!(crate::VERSION))
            );
        }
    }

    #[test]
    fn status_is_retained_and_telemetry_is_not() {
        // Status is published on change and must outlive a late subscriber; telemetry is replaced within
        // seconds, and a retained copy would be read as current long after the device went away.
        assert!(StatePayload::status(true).retained("heliobridge/x/status").retain);
        assert!(!StatePayload::default().publication("heliobridge/x/state").retain);
    }

    #[test]
    fn an_object_serialises_to_what_a_value_template_can_read() {
        let view = TelemetryView {
            timestamp: None,
            readings: vec![reading("ac_power", "-100")],
        };
        let bytes = StatePayload::telemetry(&view, &fields()).into_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(parsed["ac_power"], json!(-100));
    }

    #[test]
    fn the_field_set_covers_the_whole_catalogue() {
        let entities = Catalogue::default().entities(&Growatt);
        let fields = Fields::of(&entities);
        assert_eq!(fields.len(), entities.len(), "two entities share a key");
        assert!(!fields.is_empty());
        for entity in &entities {
            assert!(fields.contains(entity.key), "{}", entity.key);
        }
    }
}
