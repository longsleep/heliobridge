//! Which Home Assistant entity each register becomes.
//!
//! Almost nothing here is a table. The register maps already say what a setting accepts — a range with
//! bounds, a flag, a time of day, a set of labels — and that is exactly what decides the component and its
//! configuration, so the mapping is derived. A parallel table would be a second place to add a register
//! to, and would eventually disagree with the first.
//!
//! What cannot be derived is the handful of judgements Home Assistant needs and the protocol does not
//! have: which quantity a unit represents, whether a reading accumulates or is instantaneous, whether an
//! entity belongs on the dashboard or in the diagnostics block, and where a bare register name would
//! understate what a switch does.

use core::fmt;

use crate::growatt::v7::registers::{Domain, HoldingRegister, InputRegister, Kind};
use crate::model::{Confidence, Unit};

/// The Home Assistant component an entity is published as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// A numeric reading.
    Sensor,
    /// An on/off reading.
    BinarySensor,
    /// A numeric setting.
    Number,
    /// An on/off setting.
    Switch,
    /// A setting chosen from a fixed set.
    Select,
    /// A free-text setting, used where there is no better component.
    Text,
    /// A momentary action.
    Button,
}

impl Component {
    /// The name Home Assistant knows it by, which is also its place in the discovery topic.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sensor => "sensor",
            Self::BinarySensor => "binary_sensor",
            Self::Number => "number",
            Self::Switch => "switch",
            Self::Select => "select",
            Self::Text => "text",
            Self::Button => "button",
        }
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an entity appears on the device page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A setting: the configuration block rather than the dashboard.
    Config,
    /// Something about the equipment rather than the energy: versions, signal strength, connectivity.
    Diagnostic,
}

impl Category {
    /// The value of `entity_category`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Diagnostic => "diagnostic",
        }
    }
}

/// How a value behaves over time, for long-term statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateClass {
    /// An instantaneous reading.
    Measurement,
    /// A counter that only rises, apart from resets the recorder handles.
    TotalIncreasing,
}

impl StateClass {
    /// The value of `state_class`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measurement => "measurement",
            Self::TotalIncreasing => "total_increasing",
        }
    }
}

/// What a numeric setting accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Smallest accepted value.
    pub min: u16,
    /// Largest accepted value.
    pub max: u16,
}

/// The parts of an entity that differ by component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A reading, with how it behaves over time.
    Reading(Option<StateClass>),
    /// A numeric setting and its bounds.
    Numeric(Bounds),
    /// An on/off setting.
    Toggle,
    /// A setting chosen from labels.
    Choice(&'static [&'static str]),
    /// A time of day, `HH:MM`.
    TimeOfDay,
    /// A momentary action.
    Action,
}

/// One Home Assistant entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Field name, unique per device. Appears in the discovery topic and in the state payload.
    pub key: &'static str,
    /// What a person sees.
    pub name: String,
    /// Which component it is published as.
    pub component: Component,
    /// Which quantity it measures, where Home Assistant recognises one.
    pub device_class: Option<&'static str>,
    /// Unit symbol, where it has one.
    pub unit: Option<&'static str>,
    /// Where it appears on the device page.
    pub category: Option<Category>,
    /// The component-specific part.
    pub shape: Shape,
}

impl Entity {
    /// The entity a writable setting becomes.
    ///
    /// The component follows from the domain, which is the same thing the encoder validates against — so
    /// an entity can never offer a value the device would refuse.
    pub fn for_setting(register: &HoldingRegister) -> Self {
        let (component, shape) = match register.domain {
            Domain::Range { min, max } => (Component::Number, Shape::Numeric(Bounds { min, max })),
            Domain::Flag => (Component::Switch, Shape::Toggle),
            Domain::TimeOfDay => (Component::Text, Shape::TimeOfDay),
            Domain::Enum(labels) => (Component::Select, Shape::Choice(labels)),
        };

        Self {
            key: register.name,
            name: label(register.name),
            component,
            device_class: None,
            unit: symbol(register.unit),
            // Every setting is configuration, so none of them clutter the dashboard.
            category: Some(Category::Config),
            shape,
        }
    }

    /// The entity a telemetry register becomes, or `None` for one that should not be published.
    ///
    /// A register whose meaning is not established is not published: a value nobody can interpret is
    /// noise on a dashboard, and it stays available through the control API for investigation.
    pub fn for_reading(register: &InputRegister) -> Option<Self> {
        if register.name.starts_with("unknown_") {
            return None;
        }
        // Text registers are the serial, split across four of them. The device already carries its serial
        // as its identity, so publishing the pieces would add four entities saying what one already says.
        if matches!(register.kind, Kind::Text { .. }) {
            return None;
        }

        // A label has no quantity behind it. Home Assistant rejects a state class on one, and there is
        // nothing to average or accumulate in a work mode in any case.
        let numeric = matches!(register.kind, Kind::Int | Kind::Float | Kind::Float32);
        let device_class = numeric
            .then(|| device_class(register.name, register.unit))
            .flatten()
            .or_else(|| matches!(register.kind, Kind::Enum(_)).then_some("enum"));

        Some(Self {
            key: register.name,
            name: label(register.name),
            component: Component::Sensor,
            device_class,
            unit: numeric.then(|| symbol(register.unit)).flatten(),
            category: diagnostic(register.name, register.confidence),
            shape: Shape::Reading(numeric.then(|| state_class(device_class))),
        })
    }

    /// The battery pack this entity describes, if it describes one.
    ///
    /// Packs are numbered from 1, and the device reports how many are attached. A pack that is not
    /// attached still occupies its registers, and they read zero — which becomes 0 % for a state of charge
    /// and **−273.1 °C** for a temperature, since zero kelvin is what the temperature scaling makes of a
    /// zero raw value. Publishing absolute zero as a reading is worse than publishing nothing.
    pub fn battery_pack(&self) -> Option<u16> {
        let rest = self.key.strip_prefix("battery")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }

    /// Whether this entity accepts commands.
    pub const fn is_writable(&self) -> bool {
        matches!(
            self.shape,
            Shape::Numeric(_) | Shape::Toggle | Shape::Choice(_) | Shape::TimeOfDay | Shape::Action
        )
    }
}

/// The Home Assistant device class for a reading.
///
/// From the unit, with the name deciding the cases a unit cannot: watts are power and watt-hours are
/// energy, but a percentage is a state of charge on one register and a limit on another.
fn device_class(name: &str, unit: Unit) -> Option<&'static str> {
    match unit {
        Unit::Watt => Some("power"),
        Unit::KilowattHour => Some("energy"),
        Unit::Volt => Some("voltage"),
        Unit::Ampere => Some("current"),
        Unit::Celsius => Some("temperature"),
        Unit::Second => Some("duration"),
        // `battery` means state of charge specifically. A percentage that is a limit or a signal quality
        // is left without a class rather than mislabelled.
        Unit::Percent if name.contains("soc") || name.contains("soh") => Some("battery"),
        Unit::Percent | Unit::None => None,
    }
}

/// How a reading behaves over time.
///
/// Energy accumulates; everything else is instantaneous. Nothing else may be `total_increasing`: the
/// recorder treats a fall as a counter reset and counts the whole next value as new.
fn state_class(device_class: Option<&'static str>) -> StateClass {
    if device_class == Some("energy") {
        StateClass::TotalIncreasing
    } else {
        StateClass::Measurement
    }
}

/// Whether a reading belongs in the diagnostics block rather than on the dashboard.
fn diagnostic(name: &str, confidence: Confidence) -> Option<Category> {
    // Anything about the equipment rather than the energy, plus anything whose meaning rests on a name
    // inherited from another implementation rather than on something observed here.
    let equipment = name.contains("version")
        || name.contains("signal")
        || name.contains("serial")
        || name.contains("status")
        || name.contains("cell")
        || name.contains("household");
    (equipment || confidence == Confidence::Observed).then_some(Category::Diagnostic)
}

/// The unit symbol, or `None` where there is none.
fn symbol(unit: Unit) -> Option<&'static str> {
    match unit {
        Unit::None => None,
        unit => Some(unit.symbol()),
    }
}

/// A field name as a person would write it.
///
/// `charge_limit_upper` becomes `Charge limit upper`. Names that would understate what a setting does are
/// spelled out instead, because the device page is where someone decides whether to touch it — and one of
/// these disconnects the inverter from the grid.
fn label(name: &str) -> String {
    if let Some(spelled) = NAMED.iter().find(|(field, _)| *field == name) {
        return spelled.1.to_owned();
    }

    let mut out = String::with_capacity(name.len());
    for (index, word) in name.split('_').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&word_label(word, index == 0));
    }
    out
}

/// One word of a field name, capitalised as a person would write it.
fn word_label(word: &str, first: bool) -> String {
    // An acronym stays an acronym wherever it appears, including where a digit is stuck to it: `pv1`
    // reads as `PV1`, not `Pv1`.
    let letters: String = word.chars().take_while(char::is_ascii_alphabetic).collect();
    if ACRONYMS.contains(&letters.as_str()) {
        let mut out = letters.to_uppercase();
        out.push_str(word.get(letters.len()..).unwrap_or_default());
        return out;
    }

    let mut characters = word.chars();
    match characters.next() {
        Some(initial) if first => {
            let mut out: String = initial.to_uppercase().collect();
            out.push_str(characters.as_str());
            out
        }
        _ => word.to_owned(),
    }
}

/// Words that are acronyms rather than words, whatever their position.
const ACRONYMS: &[&str] = &["ac", "dc", "pv", "soc", "soh", "usb", "id", "ip"];

/// Fields whose name would read badly or understate what they do.
///
/// Two of these matter beyond tidiness: the device page is where someone decides whether to touch a
/// switch, and one of them disconnects the inverter from the grid.
const NAMED: &[(&str, &str)] = &[
    ("off_grid_mode", "Off-grid mode (stops AC output)"),
    ("power_plus", "Power+ (raises the output ceiling)"),
    ("anti_backflow_enabled", "Export limitation"),
    ("anti_backflow_power_percent", "Export limit"),
    ("grid_power_allowed", "Grid charging allowed"),
    ("always_on", "Always on"),
    ("battery_soc_total", "Battery state of charge"),
    ("battery_soh", "Battery health"),
    ("battery_charge_status", "Battery status"),
    ("battery_charge_power", "Battery power"),
    ("battery_charge_energy_today", "Battery charged today"),
    ("battery_discharge_energy_today", "Battery discharged today"),
    ("ac_output_energy_today", "AC output today"),
    ("ac_output_energy_today_2", "AC output today (duplicate)"),
    ("energy_today", "Energy today"),
    ("pv_power_total", "Solar power"),
    ("household_load_total", "Household load"),
    ("household_load_excl_groplug", "Household load, excluding plugs"),
    ("charge_limit_upper", "Charge limit, upper"),
    ("charge_limit_lower", "Charge limit, lower"),
    ("default_output_power", "Output power"),
    ("device_temp", "Device temperature"),
    ("battery1_temp", "Battery temperature"),
    ("wifi_signal", "Wi-Fi signal"),
];

#[cfg(test)]
mod tests {
    use super::{Category, Component, Entity, Shape, StateClass};
    use crate::growatt::v7::registers::{HOLDING_REGISTERS, HoldingRegister, INPUT_REGISTERS, InputRegister, Kind};
    use crate::model::Register;

    /// A setting by name, from the listed registers or from the generated slot ones.
    fn setting(name: &str) -> Entity {
        if let Some(register) = HOLDING_REGISTERS.iter().find(|entry| entry.name == name) {
            return Entity::for_setting(register);
        }
        for slot in 1..=9u16 {
            for register in HoldingRegister::slot(slot).expect("a slot in range") {
                if register.name == name {
                    return Entity::for_setting(&register);
                }
            }
        }
        panic!("no setting {name}")
    }

    fn reading(name: &str) -> Entity {
        let register = INPUT_REGISTERS
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no reading {name}"));
        Entity::for_reading(register).expect("published")
    }

    #[test]
    fn a_ranged_setting_becomes_a_number_carrying_the_registers_own_bounds() {
        // The bounds are the encoder's, not a second copy: an entity cannot offer a value the device
        // would refuse, because both come from the register map.
        let entity = setting("charge_limit_upper");
        assert_eq!(entity.component, Component::Number);
        assert_eq!(entity.category, Some(Category::Config));
        assert_eq!(entity.unit, Some("%"));
        match entity.shape {
            Shape::Numeric(bounds) => {
                assert_eq!((bounds.min, bounds.max), (70, 100));
            }
            other => panic!("expected a numeric shape, got {other:?}"),
        }
    }

    #[test]
    fn each_domain_picks_its_own_component() {
        assert_eq!(setting("always_on").component, Component::Switch);
        assert_eq!(setting("slot1_work_mode").component, Component::Select);
        assert_eq!(setting("slot1_start_time").component, Component::Text);
        assert_eq!(setting("slot1_output_power").component, Component::Number);
    }

    #[test]
    fn a_work_mode_offers_the_labels_the_device_uses() {
        match setting("slot1_work_mode").shape {
            Shape::Choice(labels) => assert!(labels.len() >= 3, "{labels:?}"),
            other => panic!("expected a choice, got {other:?}"),
        }
    }

    #[test]
    fn only_energy_accumulates() {
        // A `total_increasing` reading that is not a counter would make the Energy dashboard read a fall
        // as a reset and count the next value as fresh energy.
        assert_eq!(
            reading("pv_energy_today").shape,
            Shape::Reading(Some(StateClass::TotalIncreasing))
        );
        assert_eq!(reading("ac_power").shape, Shape::Reading(Some(StateClass::Measurement)));
        assert_eq!(
            reading("battery_soc_total").shape,
            Shape::Reading(Some(StateClass::Measurement))
        );
    }

    #[test]
    fn device_classes_follow_the_unit_except_where_it_cannot() {
        assert_eq!(reading("ac_power").device_class, Some("power"));
        assert_eq!(reading("pv_energy_today").device_class, Some("energy"));
        assert_eq!(reading("battery1_temp").device_class, Some("temperature"));
        // A percentage that is a charge level, against one that is not.
        assert_eq!(reading("battery_soc_total").device_class, Some("battery"));
        assert_eq!(setting("anti_backflow_power_percent").device_class, None);
    }

    #[test]
    fn per_pack_entities_name_the_pack_they_describe() {
        // What lets an absent pack be left unpublished: its registers read zero, which the temperature
        // scaling turns into absolute zero.
        assert_eq!(reading("battery1_soc").battery_pack(), Some(1));
        assert_eq!(reading("battery2_temp").battery_pack(), Some(2));
        assert_eq!(reading("battery4_soc").battery_pack(), Some(4));

        // Battery-wide readings belong to no single pack and are always published.
        for key in ["battery_soc_total", "battery_charge_power", "battery_cycles"] {
            assert_eq!(reading(key).battery_pack(), None, "{key}");
        }
    }

    #[test]
    fn an_absent_pack_would_report_absolute_zero() {
        // The reason the pack count is consulted at all, pinned so nobody removes the check as redundant.
        let entry = INPUT_REGISTERS
            .iter()
            .find(|entry| entry.name == "battery2_temp")
            .expect("battery2_temp");
        let absent = entry.decode(crate::model::Raw(0));
        match absent {
            crate::model::Value::Float(celsius) => {
                assert!(celsius < -273.0, "an unattached pack decodes to {celsius} °C");
            }
            other => panic!("expected a temperature, got {other:?}"),
        }
    }

    #[test]
    fn a_label_is_a_sensor_with_nothing_to_measure() {
        // Home Assistant refuses a state class on a non-numeric state, and there is nothing to average or
        // accumulate in a work mode anyway.
        let mode = reading("work_mode");
        assert_eq!(mode.shape, Shape::Reading(None));
        assert_eq!(mode.device_class, Some("enum"));
        assert_eq!(mode.unit, None);
    }

    #[test]
    fn every_measured_reading_carries_a_state_class_and_every_labelled_one_does_not() {
        for register in INPUT_REGISTERS {
            let Some(entity) = Entity::for_reading(register) else {
                continue;
            };
            let Shape::Reading(state_class) = entity.shape else {
                panic!("{} is not a reading", entity.key);
            };
            let numeric = matches!(register.kind, Kind::Int | Kind::Float | Kind::Float32);
            assert_eq!(
                state_class.is_some(),
                numeric,
                "{} has the wrong state class for its kind",
                entity.key
            );
        }
    }

    #[test]
    fn a_register_nobody_can_interpret_is_not_published() {
        let unknown = INPUT_REGISTERS
            .iter()
            .find(|entry| entry.name.starts_with("unknown_"))
            .expect("the map carries unknown registers");
        assert_eq!(Entity::for_reading(unknown), None);
    }

    #[test]
    fn the_switch_that_stops_output_says_so() {
        // It sits in a list of otherwise harmless switches, and the device page is where someone decides
        // whether to touch it.
        assert_eq!(setting("off_grid_mode").name, "Off-grid mode (stops AC output)");
    }

    #[test]
    fn names_read_as_prose() {
        // Derived from the field name where that reads well ...
        assert_eq!(setting("slot1_output_power").name, "Slot1 output power");
        // ... with acronyms kept as acronyms, including where a digit is stuck to one ...
        assert_eq!(reading("pv1_voltage").name, "PV1 voltage");
        assert_eq!(reading("ac_power").name, "AC power");
        // ... and spelled out where the field name would read badly.
        assert_eq!(reading("battery_soc_total").name, "Battery state of charge");
        assert_eq!(setting("charge_limit_upper").name, "Charge limit, upper");
    }

    #[test]
    fn every_writable_register_becomes_a_writable_entity() {
        // The catalogue must cover the whole allowlist: a setting the API accepts but Home Assistant
        // cannot reach would be a silent gap between the two interfaces.
        for register in HOLDING_REGISTERS {
            let entity = Entity::for_setting(register);
            assert!(entity.is_writable(), "{} is not writable", register.name);
            assert_eq!(entity.key, register.name);
        }
        // Slots are generated rather than listed, so they are checked separately.
        for slot in 1..=9u16 {
            for register in HoldingRegister::slot(slot).expect("a slot in range") {
                assert!(Entity::for_setting(&register).is_writable(), "slot {slot}");
            }
        }
    }

    #[test]
    fn every_named_reading_becomes_a_sensor() {
        let published = INPUT_REGISTERS.iter().filter_map(Entity::for_reading).count();
        let named = INPUT_REGISTERS
            .iter()
            .filter(|entry| !entry.name.starts_with("unknown_") && !matches!(entry.kind, Kind::Text { .. }))
            .count();
        assert_eq!(published, named);
        assert!(named > 30, "only {named} named registers");
    }

    #[test]
    fn a_reading_is_never_a_setting_by_accident() {
        // Every telemetry entity is read-only; the two maps must not overlap in what they offer.
        for register in INPUT_REGISTERS {
            if let Some(entity) = Entity::for_reading(register) {
                assert!(!entity.is_writable(), "{} should be read-only", register.name);
            }
        }
        let _ = InputRegister::lookup(Register(5)).expect("ac_power exists");
    }
}
