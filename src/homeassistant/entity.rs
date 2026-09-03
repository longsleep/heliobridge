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
use std::collections::HashSet;

use crate::growatt::v7::registers::{Domain, HoldingRegister, INPUT_REGISTERS, InputRegister, Kind, SLOT_COUNT};
use crate::homeassistant::command::Permitted;
use crate::model::{Scaling, Unit};

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

/// Which topic carries an entity's state.
///
/// Three, because they are published on three different schedules: telemetry every few seconds from the
/// device, settings when one changes or is read back, and status whenever this bridge's *opinion* of the
/// device changes. The last has to keep being published when the first stops, which is the whole reason it
/// is not a field inside the telemetry object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A decoded telemetry frame.
    Telemetry,
    /// Holding-register values, as last read back.
    Settings,
    /// What this bridge knows about the device rather than from it.
    Status,
    /// Datalogger configuration, from the identity report the device sends on connect.
    ///
    /// A fourth address space rather than a fourth flavour of the same one, and mostly not entity
    /// material: most of it is static, inert, or identifying. The handful published here are the ones
    /// that change and mean something.
    ///
    /// **Only a register the device volunteers may become an entity.** Most of the config space answers
    /// an explicit read and nothing else, so an entity built on one would sit unknown until somebody
    /// asked — announcing something Home Assistant can never fill in on its own. A test below holds this.
    Config,
}

/// What an entity's availability depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// This program running *and* the device reporting. Everything that carries a reading or a setting:
    /// there is no honest value for one of these while the device is away.
    Device,
    /// This program running, whatever the device is doing. Only for the entities whose job is to report
    /// that the device is away — listing the device's own availability would make them disappear at the
    /// moment they became worth reading.
    Bridge,
}

/// A condition, beyond the device reporting at all, under which an entity has no honest value.
///
/// Availability rather than a state: these are entities whose value would otherwise look perfectly
/// ordinary while meaning nothing. A superseded setting reads back exactly what was written to it, and a
/// meter reading of `0` looks like a measurement — so in both cases nothing in the value itself reveals
/// that it is inert, and going unavailable is the only way to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Available only while another *setting* holds something other than `value`.
    SettingIsNot {
        /// The setting that decides.
        setting: &'static str,
        /// The value under which this entity is inert.
        value: u16,
    },
    /// Available only while a *telemetry* reading is non-zero.
    ReadingIsSet {
        /// The reading that decides.
        reading: &'static str,
    },
}

/// When the last telemetry frame arrived, which does not exist until one has.
pub const LAST_UPDATE: &str = "last_update";

/// The reading that says whether the device currently has a meter reporting.
pub const METER_CONNECTED: &str = "meter_connected";

/// The control that supplies a meter reading to the device.
pub const METER_READING: &str = "supplied_meter_reading";

/// The control that withdraws it.
pub const WITHDRAW_METER_READING: &str = "withdraw_meter_reading";

/// What a numeric control accepts.
///
/// Signed, though every *setting* the device holds is unsigned: a supplied meter reading is a signed
/// quantity, since export is negative, and it is published as a number like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Smallest accepted value.
    pub min: i32,
    /// Largest accepted value.
    pub max: i32,
}

/// The parts of an entity that differ by component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A reading, with how it behaves over time.
    Reading(Option<StateClass>),
    /// A word of flags, rendered as hexadecimal.
    ///
    /// A bitfield is not a quantity: `0` is not a count of faults, and `1024` is not more of anything than
    /// `512`. Hexadecimal says so at a glance, and it is what the specification's bit tables are indexed
    /// by, so a word that appears can be looked up rather than converted first.
    Flags,
    /// An on/off reading, with the payloads that mean each.
    Signal {
        /// Payload meaning on.
        on: &'static str,
        /// Payload meaning off.
        off: &'static str,
    },
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
    /// How many decimals the reading actually resolves, where it is a measurement.
    ///
    /// Home Assistant otherwise picks a default from the unit, and its default for volts is *none* — which
    /// renders a cell voltage of 3.325 V as `3 V`.
    pub precision: Option<u8>,
    /// The component-specific part.
    pub shape: Shape,
    /// Which topic carries its state, or `None` for a control that has none.
    ///
    /// `None` makes the control *optimistic*: Home Assistant shows the value it last set, because
    /// nothing reports the value back. Only correct where the device offers no read-back and something
    /// else carries the truth — a supplied meter reading is reported by the device as
    /// `meter_active_power`, so that sensor is the feedback rather than an echo of what was written.
    pub source: Option<Source>,
    /// What its availability depends on.
    pub presence: Presence,
    /// A further condition for it to be available, where it has one.
    pub gate: Option<Gate>,
}

impl Entity {
    /// The entity a writable setting becomes.
    ///
    /// The component follows from the domain, which is the same thing the encoder validates against — so
    /// an entity can never offer a value the device would refuse.
    pub fn for_setting(register: &HoldingRegister) -> Self {
        let (component, shape) = match register.domain {
            Domain::Range { min, max } => (
                Component::Number,
                Shape::Numeric(Bounds {
                    min: i32::from(min),
                    max: i32::from(max),
                }),
            ),
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
            // A setting is a whole number of watts, percent or minutes; its step carries the resolution.
            precision: None,
            shape,
            source: Some(Source::Settings),
            presence: Presence::Device,
            gate: register.superseded_by.map(|by| Gate::SettingIsNot {
                setting: by.setting,
                value: by.when,
            }),
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

        // A flags word and a label share this much: neither is a quantity, so neither may carry a unit, a
        // device class or a state class. `0` faults is not a measurement of nothing.
        let flags = is_flags(register.name);
        let signal = is_signal(register.name);
        let numeric = !flags && !signal && matches!(register.kind, Kind::Int | Kind::Float | Kind::Float32);
        let device_class = numeric
            .then(|| device_class(register.name, register.unit))
            .flatten()
            .or_else(|| matches!(register.kind, Kind::Enum(_)).then_some("enum"));

        Some(Self {
            key: register.name,
            name: label(register.name),
            component: if signal {
                Component::BinarySensor
            } else {
                Component::Sensor
            },
            device_class: if signal { Some("connectivity") } else { device_class },
            unit: numeric.then(|| symbol(register.unit)).flatten(),
            category: diagnostic(register.name),
            precision: numeric.then(|| precision(register.scaling)),
            shape: if flags {
                Shape::Flags
            } else if signal {
                Shape::Signal { on: "1", off: "0" }
            } else {
                Shape::Reading(numeric.then(|| state_class(device_class)))
            },
            source: Some(Source::Telemetry),
            presence: Presence::Device,
            gate: register.gated_by.map(|reading| Gate::ReadingIsSet { reading }),
        })
    }

    /// Whether the device is reporting.
    ///
    /// One of the entities that must survive the outage it describes, hence [`Presence::Bridge`], and an
    /// ordinary entity rather than a diagnostic for the same reason as [`Self::last_update`]: it qualifies
    /// every reading on the page, so it belongs beside them.
    pub fn connected() -> Self {
        Self {
            key: "connected",
            name: "Device connected".to_owned(),
            component: Component::BinarySensor,
            device_class: Some("connectivity"),
            unit: None,
            category: None,
            precision: None,
            shape: Shape::Signal {
                on: "online",
                off: "offline",
            },
            source: Some(Source::Status),
            presence: Presence::Bridge,
            gate: None,
        }
    }

    /// When the most recent telemetry frame arrived.
    ///
    /// Answers "how stale is this?", which is the question a dashboard needs when a value looks wrong, and
    /// it is the one thing that stays truthful while every reading is unavailable. That is why it is an
    /// ordinary sensor rather than a diagnostic: it is read alongside the readings it qualifies.
    pub fn last_update() -> Self {
        Self {
            key: LAST_UPDATE,
            name: "Last update".to_owned(),
            component: Component::Sensor,
            device_class: Some("timestamp"),
            unit: None,
            category: None,
            precision: None,
            // No state class: Home Assistant refuses one on a timestamp, and there is nothing to average.
            shape: Shape::Reading(None),
            source: Some(Source::Status),
            presence: Presence::Bridge,
            gate: None,
        }
    }

    /// Which build of this program produced the readings.
    ///
    /// A diagnostic, unlike the other two on the status topic: it says nothing about the energy or about
    /// how current anything is. It is here because a device page that reports a firmware version and a
    /// hardware version, and stays silent about the software actually doing the decoding, sends the reader
    /// looking in the wrong place. Every field on that page is this program's interpretation of a register.
    ///
    /// [`Presence::Bridge`] for the obvious reason: it describes the bridge, so a device that has gone away
    /// has no bearing on whether it is true.
    pub fn bridge_version() -> Self {
        Self {
            key: "bridge_version",
            // Named for the program rather than "Bridge version", which on a page full of the *device's*
            // versions would read as another of them.
            name: "Heliobridge version".to_owned(),
            component: Component::Sensor,
            device_class: None,
            unit: None,
            category: Some(Category::Diagnostic),
            precision: None,
            shape: Shape::Reading(None),
            source: Some(Source::Status),
            presence: Presence::Bridge,
            gate: None,
        }
    }

    /// How strong the datalogger's Wi-Fi is, in dBm.
    ///
    /// Config register 76, which the device volunteers on every connect, so this costs no traffic. Verified
    /// against the vendor's own web interface, which showed "Good(-72)" while the register read -72 — the
    /// unit is dBm and the sign is as sent.
    pub fn wifi_signal() -> Self {
        Self {
            key: "wifi_signal",
            name: "Wi-Fi signal".to_owned(),
            component: Component::Sensor,
            device_class: Some("signal_strength"),
            unit: Some("dBm"),
            category: Some(Category::Diagnostic),
            precision: Some(0),
            shape: Shape::Reading(Some(StateClass::Measurement)),
            source: Some(Source::Config),
            presence: Presence::Device,
            gate: None,
        }
    }

    /// How often the device says it reports, in seconds.
    ///
    /// Config register 4. It qualifies [`Self::last_update`]: how stale a reading is only means something
    /// against how often one is expected. Every frame ever captured says 5.
    pub fn data_interval() -> Self {
        Self {
            key: "data_interval",
            name: "Telemetry interval".to_owned(),
            component: Component::Sensor,
            device_class: None,
            unit: Some("s"),
            category: Some(Category::Diagnostic),
            precision: Some(0),
            shape: Shape::Reading(None),
            source: Some(Source::Config),
            presence: Presence::Device,
            gate: None,
        }
    }

    /// Restart the datalogger.
    ///
    /// Config register 32, the one action worth offering: the device reboots, reconnects by itself within
    /// seconds, and the inverter keeps running throughout — so the cost of an accidental press is a gap in
    /// telemetry. Its sibling, the factory reset of register 35, is deliberately **not** published: it
    /// clears the Wi-Fi credentials, and recovering from that means standing next to the device with a
    /// Bluetooth client.
    ///
    /// A button carries no state, so unlike the other config entities this names a register the device
    /// never reports — there is nothing to report. Home Assistant is told not to enable it by default;
    /// rebooting hardware should be a deliberate act.
    pub fn restart() -> Self {
        Self {
            key: "restart",
            name: "Restart datalogger".to_owned(),
            component: Component::Button,
            device_class: Some("restart"),
            unit: None,
            category: Some(Category::Config),
            precision: None,
            shape: Shape::Action,
            source: Some(Source::Config),
            presence: Presence::Device,
            gate: None,
        }
    }

    /// The meter reading to supply to the device.
    ///
    /// Optimistic, because the device offers no read-back for these registers. The feedback is the
    /// `meter_active_power` sensor — the device's own report of the reading it currently holds, which
    /// falls to zero by itself when a reading expires. So Home Assistant shows what it asked for, and the
    /// sensor beside it shows what the device is acting on; the two disagreeing is exactly the signal that
    /// something has stopped writing.
    ///
    /// **This is not a setting, and writing it once achieves nothing lasting.** A reading expires after
    /// about two minutes, so whatever drives this has to write it again inside that window, from a figure
    /// it has actually measured. Nothing in this program refreshes it.
    ///
    /// Bounds are wider than the equipment can do anything with, deliberately: the reading describes the
    /// *house*, not the device, and a household can import far more than a 2 kW inverter can offset.
    pub fn meter_reading() -> Self {
        Self {
            key: METER_READING,
            name: "Supplied meter reading".to_owned(),
            component: Component::Number,
            device_class: Some("power"),
            unit: Some("W"),
            category: Some(Category::Config),
            precision: None,
            shape: Shape::Numeric(Bounds {
                min: -20_000,
                max: 20_000,
            }),
            source: None,
            presence: Presence::Device,
            // Deliberately ungated. Writing a reading is what *makes* the device hold one, so gating this
            // on the device already holding one would leave the only way in permanently unavailable.
            // Whether a reading is in effect is reported by `meter_connected` beside it.
            gate: None,
        }
    }

    /// Withdraw the supplied reading, telling the device its meter has gone.
    ///
    /// A button rather than the off half of a switch: there is no meaningful "on" to pair it with, since
    /// arming without a figure would supply nothing. It writes the all-zero block the firmware itself
    /// writes for a meter that stopped answering, which drops the reading at once instead of waiting out
    /// the two-minute expiry.
    ///
    /// Distinct from supplying `0`, which is a *valid* reading meaning the grid is balanced — and which
    /// the device acts on by holding its output where it is.
    pub fn withdraw_meter_reading() -> Self {
        Self {
            key: WITHDRAW_METER_READING,
            name: "Withdraw meter reading".to_owned(),
            component: Component::Button,
            device_class: None,
            unit: None,
            category: Some(Category::Config),
            precision: None,
            shape: Shape::Action,
            source: None,
            presence: Presence::Device,
            // Nothing to withdraw when the device holds no reading.
            gate: Some(Gate::ReadingIsSet {
                reading: METER_CONNECTED,
            }),
        }
    }

    /// The serial the device identifies itself by.
    ///
    /// Already the device page's `serial_number`, and repeated as an entity because that field cannot be
    /// templated against while an entity can. Config register 8, which is also the MQTT client identifier.
    pub fn serial_number() -> Self {
        Self {
            key: "serial_number",
            name: "Serial number".to_owned(),
            component: Component::Sensor,
            device_class: None,
            unit: None,
            category: Some(Category::Diagnostic),
            precision: None,
            shape: Shape::Reading(None),
            source: Some(Source::Config),
            presence: Presence::Device,
            gate: None,
        }
    }

    /// The same entity with nothing writable about it.
    ///
    /// What `HELIOBRIDGE_ALLOW_WRITES=false` produces: a setting still worth seeing, published as a plain
    /// reading so Home Assistant offers no control that would be refused. A read-only `number` would still
    /// draw a spinbox.
    #[must_use]
    pub fn into_read_only(self) -> Self {
        if !self.is_writable() {
            return self;
        }
        Self {
            component: Component::Sensor,
            // Nothing here accumulates: these are positions and limits, not counters.
            shape: Shape::Reading(None),
            ..self
        }
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

    /// Whether this entity is published on a topic of its own rather than reading a shared object.
    ///
    /// **Home Assistant does not silently ignore a state that templates to nothing.** A `text` entity
    /// fails the length and pattern it was announced with, and a `sensor` logs `Invalid state message ''`
    /// — both observed. So a field that is sometimes *absent* from its object must not share that object
    /// with fields that are always present: on a topic of its own, nothing is published until there is
    /// something to publish, and the entity simply stays unknown.
    ///
    /// Two fields are absent for a while:
    ///
    /// - a slot boundary, because the settings cache fills in one register at a time over the half-minute
    ///   a resync takes;
    /// - [`LAST_UPDATE`], which does not exist until a frame has arrived.
    pub fn published_alone(&self) -> bool {
        matches!(self.shape, Shape::TimeOfDay) || self.key == LAST_UPDATE
    }

    /// Whether this entity accepts commands.
    pub const fn is_writable(&self) -> bool {
        matches!(
            self.shape,
            Shape::Numeric(_) | Shape::Toggle | Shape::Choice(_) | Shape::TimeOfDay | Shape::Action
        )
    }
}

/// How many battery packs the register map describes.
///
/// The device reports how many are attached; this is the ceiling the registers provide for, so it is also
/// how far a reconciliation has to look for entities left over from a larger installation.
pub const BATTERY_PACKS: u16 = 4;

/// Which entities one device gets.
///
/// The three things that vary between installations, in one place: how much of the schedule is exposed,
/// whether this bridge is allowed to write, and how much battery is attached. All three are known only at
/// runtime, so the catalogue is built per device rather than being a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Catalogue {
    /// How many schedule slots get entities, 1–9. Each adds five.
    pub slots: u16,
    /// Which settings may be written, and so which are offered as controls.
    ///
    /// The same value the command topic is checked against, so a control is published exactly when a
    /// command for it would be honoured. A setting that may not be written still appears — as a plain
    /// reading, since whether it is on remains worth seeing even where this bridge may not change it.
    pub permitted: Permitted,
    /// How many battery packs get entities.
    ///
    /// What the device reports, and **one** until it has: it is a battery, so it has at least that. There
    /// is deliberately no "unknown" here — it would produce a second catalogue identical to this one, and a
    /// second announcement of it.
    pub packs: u16,
}

impl Default for Catalogue {
    fn default() -> Self {
        Self {
            slots: 1,
            permitted: Permitted::default(),
            packs: 1,
        }
    }
}

impl Catalogue {
    /// Every entity this device should have, in a stable order.
    ///
    /// The settings are exactly the resync set — the same registers the session reads back on connect — so
    /// every published setting entity has a value behind it rather than sitting unavailable forever.
    pub fn entities(self) -> Vec<Entity> {
        let reported: HashSet<&'static str> = INPUT_REGISTERS.iter().map(|register| register.name).collect();

        let settings: Vec<Entity> = HoldingRegister::resync_set(self.slots.min(SLOT_COUNT))
            .into_iter()
            .map(|register| {
                let mut entity = Entity::for_setting(&register);
                // The charge limits are the two fields that exist in both address spaces: a writable
                // holding register, and an input register carried in every telemetry frame. One entity,
                // then — a duplicate key is an entity Home Assistant drops without saying so — and it
                // reads from telemetry, which is fresher by up to an hour. The settings cache learns of a
                // change made in the vendor app only from the next hourly snapshot.
                if reported.contains(register.name) {
                    entity.source = Some(Source::Telemetry);
                }
                if self.permitted.allows(register.name) {
                    entity
                } else {
                    entity.into_read_only()
                }
            })
            .collect();

        let claimed: HashSet<&'static str> = settings.iter().map(|entity| entity.key).collect();
        let readings = INPUT_REGISTERS
            .iter()
            .filter_map(Entity::for_reading)
            .filter(|entity| self.includes(entity))
            .filter(|entity| !claimed.contains(entity.key));

        readings
            .chain(settings)
            .chain([Entity::connected(), Entity::last_update(), Entity::bridge_version()])
            .chain([Entity::wifi_signal(), Entity::data_interval(), Entity::serial_number()])
            // An action has nothing to publish but a control, so refusing writes withdraws it entirely
            // rather than downgrading it to a reading the way a setting does. The meter controls go the
            // same way: both are write-only, so read-only versions of them would be entities that can
            // neither be read nor written.
            .chain(self.permitted.writes.then(Entity::restart))
            .chain(self.permitted.writes.then(Entity::meter_reading))
            .chain(self.permitted.writes.then(Entity::withdraw_meter_reading))
            .collect()
    }

    /// Entities this program once published and no longer does.
    ///
    /// [`Self::everything`] is built from the register maps, so by construction it cannot describe an
    /// entity whose constructor has been **deleted**. That makes a removal invisible to the very
    /// reconciliation meant to clean up after one: the retained discovery message stays on the broker,
    /// Home Assistant keeps the entity forever, and it survives restarts because the retained message
    /// re-creates it. Nothing in the code would ever mention it again, so nothing would ever notice.
    ///
    /// This is the memory the catalogue otherwise lacks. An entry is a `(component, key)` pair, because the
    /// component picks the discovery topic — the same reason `everything` includes both forms of a setting.
    ///
    /// **Add to this list in the same change that deletes an entity**, and leave the entry in place. It
    /// costs one empty retained publish per session on a topic that is already empty; the alternative is a
    /// stale entity on someone's dashboard that only a hand-run `mosquitto_pub` can remove.
    ///
    /// Empty, and correctly so: no released version of this program has ever announced an entity that a
    /// later one withdrew. An entity that existed only between two commits needs no entry — nobody's broker
    /// ever held it. The list is for entities that reached a release.
    pub const RETIRED: &'static [(Component, &'static str)] = &[];

    /// Every entity any configuration of this device could produce.
    ///
    /// What a reconciliation has to compare against when there is no record of what was announced before —
    /// after a restart, where the broker may still hold retained discovery from a run configured
    /// differently. Both forms of every setting are included: refusing writes changes an entity's
    /// *component*, and therefore its discovery topic, so a `switch` that became a `sensor` leaves a
    /// retained message behind under its old name.
    pub fn everything() -> Vec<Entity> {
        let widest = Self {
            slots: SLOT_COUNT,
            permitted: Permitted::default(),
            packs: BATTERY_PACKS,
        };
        // Two passes cover every component a setting can be published as: with writes refused, each control
        // becomes a sensor, and that includes the one setting whose reachability is configurable on its own.
        let mut all = widest.entities();
        all.extend(
            Self {
                permitted: Permitted {
                    writes: false,
                    ..widest.permitted
                },
                ..widest
            }
            .entities(),
        );
        all
    }

    /// Whether a reading belongs in this catalogue at all.
    ///
    /// Only per-pack readings are ever left out, and only for a pack that is not attached: its registers
    /// read zero, which the temperature scaling turns into −273.1 °C, and publishing absolute zero as a
    /// reading is worse than publishing nothing.
    fn includes(self, entity: &Entity) -> bool {
        match entity.battery_pack() {
            Some(pack) => pack <= self.packs,
            None => true,
        }
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

/// Whether a field is a word of flags rather than a measurement.
///
/// By name, as the diagnostics rule is. The register map calls these `*_faults` and the specification
/// documents them as flags words with a bit table each; nothing else in either map is a bitfield.
fn is_flags(name: &str) -> bool {
    name.ends_with("_faults")
}

/// Whether a reading is a 0/1 condition rather than a quantity.
///
/// Named rather than derived, for the same reason as [`is_flags`]: the register map says a value is a
/// 16-bit integer, and only this knows that it is really a yes or no. A binary sensor rather than a sensor
/// reading `0`, so an automation can ask `is_state(..., 'on')`.
fn is_signal(name: &str) -> bool {
    name == METER_CONNECTED
}

/// How many decimals a reading resolves, from the scaling that produced it.
///
/// The register map is the authority on this: a value scaled by a thousandth resolves to a thousandth, and
/// claiming more decimals than that would render noise as precision. Without it Home Assistant picks a
/// default from the unit, and its default for volts is none — which shows a cell voltage of 3.325 V as
/// `3 V`.
fn precision(scaling: Scaling) -> u8 {
    // Compared as ranges rather than for equality: these are the multipliers the map uses, and a float is
    // the wrong thing to test for exactness.
    if scaling.multiplier >= 1.0 {
        0
    } else if scaling.multiplier >= 0.1 {
        1
    } else if scaling.multiplier >= 0.01 {
        2
    } else {
        3
    }
}

/// Whether a reading belongs in the diagnostics block rather than on the dashboard.
///
/// What is left there is what describes the *equipment* rather than the energy: which firmware it runs,
/// how well it is reaching the network, what is wrong with it, and the cell-level detail behind the pack.
///
/// Confidence deliberately does not decide this. It once did — anything short of verified was filed as a
/// diagnostic — and the effect was that most of the device ended up in a collapsed block: the state of
/// charge of every pack but the first, every PV string's voltage and current, the daily AC output, the grid
/// voltage, the cycle count. Those are the readings someone builds a dashboard out of. How firmly a
/// field's meaning is established is still published, on every reading the control API serves and as a
/// marker in the specification, which is where a reader can act on it.
fn diagnostic(name: &str) -> Option<Category> {
    let equipment = name.contains("version")
        || name.contains("signal")
        || name.contains("serial")
        // Per-cell voltages describe the pack's internals. Useful when investigating one, noise beside a
        // reading of what the house is doing.
        || name.contains("cell")
        || name.contains("fault");
    equipment.then_some(Category::Diagnostic)
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

/// One word of a field name, as a person would write it.
fn word_label(word: &str, first: bool) -> String {
    let letters: String = word.chars().take_while(char::is_ascii_alphabetic).collect();
    let digits = word.get(letters.len()..).unwrap_or_default();

    // An abbreviation nobody says out loud is spelled out. `battery1_soc` on a device page should read
    // like the thing it is, not like a field name — and the numbered ones are the diagnostics, where a
    // reader has the least context to expand it themselves.
    if let Some(spelled) = SPELLED.iter().find(|(short, _)| *short == letters) {
        return format!("{}{digits}", spelled.1);
    }

    // An acronym stays an acronym wherever it appears, including where a digit is stuck to it: `pv1`
    // reads as `PV1`, not `Pv1`.
    if ACRONYMS.contains(&letters.as_str()) {
        return format!("{}{digits}", letters.to_uppercase());
    }

    // A word with a number stuck to it is two things: `battery1` is battery 1, and reads that way.
    let spaced = if digits.is_empty() {
        letters
    } else {
        format!("{letters} {digits}")
    };
    let mut characters = spaced.chars();
    match characters.next() {
        Some(initial) if first => {
            let mut out: String = initial.to_uppercase().collect();
            out.push_str(characters.as_str());
            out
        }
        _ => spaced,
    }
}

/// Words that are acronyms rather than words, whatever their position.
const ACRONYMS: &[&str] = &["ac", "dc", "pv", "usb", "id", "ip"];

/// Abbreviations that read as jargon and are spelled out instead.
///
/// Not in [`ACRONYMS`]: these are written short in the protocol and said long by people, so an entity name
/// carrying the short form asks the reader to know the field name.
const SPELLED: &[(&str, &str)] = &[("soc", "state of charge"), ("soh", "state of health")];

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
    ("energy_today", "Energy today"),
    ("pv_power_total", "Solar power"),
    ("household_load_total", "Household load"),
    ("household_load_excl_groplug", "Household load, excluding plugs"),
    ("charge_limit_upper", "Charge limit, upper"),
    ("charge_limit_lower", "Charge limit, lower"),
    // Every power that is an *output* says so, so that the three of them read as one family beside the
    // slot and default settings that command them. The field names are left alone: they are the
    // specification's, and appear in the state topic and in every capture.
    ("ac_power", "AC output power"),
    ("on_grid_power", "On-grid output power"),
    ("off_grid_power", "Off-grid output power"),
    ("default_output_power", "Default output power"),
    // "Grid faults: 0" reads as a count of faults. These are words of flags, and the label has to say so.
    ("internal_faults", "Internal fault flags"),
    ("grid_faults", "Grid fault flags"),
    ("output_faults", "Output fault flags"),
    ("device_temp", "Device temperature"),
    ("battery1_temp", "Battery temperature"),
    ("wifi_signal", "Wi-Fi signal"),
];

#[cfg(test)]
mod tests {
    use super::{Catalogue, Category, Component, Entity, Gate, METER_CONNECTED, Shape, Source, StateClass};
    use crate::growatt::v7::registers::{
        Availability, ConfigRegister, HOLDING_REGISTERS, HoldingRegister, INPUT_REGISTERS, InputRegister, Kind,
    };
    use crate::model::Register;

    /// Nothing is announced that the device does not volunteer.
    ///
    /// A config register that only answers an explicit read would give Home Assistant an entity it can
    /// never fill in by itself: it would sit unknown from the moment it appeared until somebody happened
    /// to ask for that register. Most of the config space is like that, so the rule needs holding
    /// structurally rather than remembering.
    #[test]
    fn every_config_entity_reads_a_register_the_device_reports() {
        // Actions are excluded because they carry no state at all: a button has no state topic, so there
        // is nothing for the device to report and no register to require in the identity report.
        let published: Vec<_> = Catalogue::everything()
            .into_iter()
            .filter(|entity| entity.source == Some(Source::Config) && entity.shape != Shape::Action)
            .collect();
        assert!(
            !published.is_empty(),
            "the config entities went missing rather than passing trivially"
        );

        for entity in published {
            let register = ConfigRegister::lookup_name(entity.key)
                .unwrap_or_else(|| panic!("{} is published but names no config register", entity.key));
            assert_eq!(
                register.availability,
                Availability::Reported,
                "{} is published but the device only answers it on request",
                entity.key
            );
        }
    }

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
    fn the_fault_words_are_diagnostics_and_never_look_like_measurements() {
        // Bitfields with two of forty-eight bits identified: worth exposing so a fault is visible at all,
        // but not on the dashboard, and with nothing about them that suggests a quantity. `Grid faults: 0`
        // reads as a count of faults, and `1024` is not more of anything than `512`.
        for key in ["internal_faults", "grid_faults", "output_faults"] {
            let entity = reading(key);
            assert_eq!(entity.category, Some(Category::Diagnostic), "{key}");
            assert_eq!(entity.unit, None, "{key}");
            assert_eq!(entity.device_class, None, "{key}");
            assert_eq!(entity.precision, None, "{key}");
            assert_eq!(entity.shape, Shape::Flags, "{key}");
        }
        assert_eq!(reading("grid_faults").name, "Grid fault flags");
    }

    #[test]
    fn a_field_in_both_address_spaces_becomes_one_entity_fed_by_telemetry() {
        // The charge limits are a writable holding register *and* an input register in every frame. Two
        // entities would share a unique_id, which Home Assistant resolves by dropping one without saying
        // so — and the telemetry copy is the fresher source, since the settings cache learns of a change
        // made elsewhere only from the next hourly snapshot.
        let entities = Catalogue::default().entities();
        for key in ["charge_limit_upper", "charge_limit_lower"] {
            let matching: Vec<&Entity> = entities.iter().filter(|entity| entity.key == key).collect();
            assert_eq!(matching.len(), 1, "{key} appears {} times", matching.len());
            let entity = matching.first().expect("just counted");
            assert_eq!(entity.component, Component::Number, "{key} must stay writable");
            assert_eq!(
                entity.source,
                Some(Source::Telemetry),
                "{key} should read the fresher source"
            );
        }
    }

    #[test]
    fn no_two_entities_claim_one_key() {
        // A duplicate is a silently missing entity, so the whole catalogue is checked rather than the two
        // fields known to overlap today.
        for slots in [1, 9] {
            let entities = Catalogue {
                slots,
                ..Catalogue::default()
            }
            .entities();
            let mut keys: Vec<&str> = entities.iter().map(|entity| entity.key).collect();
            let total = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), total, "the catalogue repeats a key with {slots} slots");
        }
    }

    #[test]
    fn a_settings_entity_reads_the_settings_topic_unless_telemetry_carries_it() {
        let entities = Catalogue::default().entities();
        let source = |key: &str| {
            entities
                .iter()
                .find(|entity| entity.key == key)
                .map(|entity| entity.source)
        };
        assert_eq!(source("always_on"), Some(Some(Source::Settings)));
        assert_eq!(source("slot1_output_power"), Some(Some(Source::Settings)));
        assert_eq!(source("ac_power"), Some(Some(Source::Telemetry)));
        assert_eq!(source("connected"), Some(Some(Source::Status)));
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
    fn every_measured_reading_carries_a_state_class_and_nothing_else_does() {
        // A state class on something that is not a quantity is what the recorder trips over: it will happily
        // average a work mode's index or read a bitfield falling to zero as a counter reset.
        for register in INPUT_REGISTERS {
            let Some(entity) = Entity::for_reading(register) else {
                continue;
            };
            // A bitfield and a yes-or-no are both 16-bit integers in the map and neither is a quantity.
            let quantity = matches!(register.kind, Kind::Int | Kind::Float | Kind::Float32)
                && !matches!(entity.shape, Shape::Flags | Shape::Signal { .. });
            match entity.shape {
                Shape::Reading(state_class) => assert_eq!(
                    state_class.is_some(),
                    quantity,
                    "{} has the wrong state class for its kind",
                    entity.key
                ),
                Shape::Flags => assert!(!quantity, "{} is a bitfield, not a quantity", entity.key),
                // A yes-or-no is not a quantity either: no unit, no state class, nothing to average.
                Shape::Signal { .. } => {
                    assert!(!quantity, "{} is a condition, not a quantity", entity.key);
                    assert!(entity.unit.is_none(), "{} is a condition and has no unit", entity.key);
                }
                other => panic!("{} is not a reading, a flags word or a signal: {other:?}", entity.key),
            }
        }
    }

    #[test]
    fn the_way_to_supply_a_reading_is_not_gated_on_a_reading_existing() {
        // The circularity this prevents: `meter_connected` is 1 only *because* a reading was supplied, so
        // gating the control that supplies one on it would leave the only way in permanently unavailable.
        let reading = Entity::meter_reading();
        assert!(reading.gate.is_none(), "the reading input must stay reachable");

        // Its two companions are gated, because both are meaningless with no reading held.
        assert_eq!(
            Entity::withdraw_meter_reading().gate,
            Some(Gate::ReadingIsSet {
                reading: METER_CONNECTED
            })
        );
        let active = INPUT_REGISTERS
            .iter()
            .find(|entry| entry.name == "meter_active_power")
            .expect("mapped");
        assert_eq!(
            Entity::for_reading(active).expect("an entity").gate,
            Some(Gate::ReadingIsSet {
                reading: METER_CONNECTED
            })
        );
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
        // Derived from the field name where that reads well, with a number that is stuck to a word read as
        // the separate thing it is ...
        assert_eq!(setting("slot1_output_power").name, "Slot 1 output power");
        assert_eq!(reading("battery2_soc").name, "Battery 2 state of charge");
        // ... acronyms kept as acronyms, including where a digit is stuck to one ...
        assert_eq!(reading("pv1_voltage").name, "PV1 voltage");
        // ... and spelled out where the field name would read badly.
        assert_eq!(reading("battery_soc_total").name, "Battery state of charge");
        assert_eq!(setting("charge_limit_upper").name, "Charge limit, upper");
    }

    #[test]
    fn a_reading_claims_exactly_the_decimals_its_scaling_resolves() {
        // Home Assistant's default for a voltage is no decimals, which rendered a cell voltage of 3.325 V
        // as `3 V`. Claiming more than the scaling resolves would be the opposite mistake.
        assert_eq!(reading("battery_cell_voltage_max").precision, Some(3));
        assert_eq!(reading("pv1_voltage").precision, Some(2));
        assert_eq!(reading("pv_energy_today").precision, Some(1));
        assert_eq!(reading("battery1_temp").precision, Some(1));
        // Whole quantities: the raw register already is the value.
        assert_eq!(reading("ac_power").precision, Some(0));
        assert_eq!(reading("battery_soc_total").precision, Some(0));
        // Nothing to round.
        assert_eq!(reading("work_mode").precision, None);
        assert_eq!(setting("charge_limit_upper").precision, None);
    }

    #[test]
    fn what_stays_in_diagnostics_is_about_the_equipment() {
        // Confidence used to decide this, which filed most of the device under diagnostics — every pack's
        // state of charge but the first, every string's voltage, the cycle count. Those are the readings a
        // dashboard is built from.
        for key in ["internal_faults", "battery_cell_voltage_max"] {
            assert_eq!(reading(key).category, Some(Category::Diagnostic), "{key}");
        }
        for key in [
            "battery2_soc",
            "pv1_voltage",
            "pv1_current",
            "device_temp",
            "grid_voltage",
            "battery_cycles",
            "battery_soh",
            "battery_charge_status",
            "battery_pack_count",
            "ac_output_energy_today",
            "pv_energy_month",
            "household_load_total",
        ] {
            assert_eq!(reading(key).category, None, "{key} belongs on the dashboard");
        }
    }

    #[test]
    fn an_abbreviation_nobody_says_out_loud_is_spelled_out() {
        // `SOC` on a diagnostics page asks the reader to know the field name. The numbered ones are exactly
        // where they have the least context to expand it themselves.
        for (key, expected) in [
            ("battery1_soc", "Battery 1 state of charge"),
            ("battery4_soc", "Battery 4 state of charge"),
            ("battery_soc_total", "Battery state of charge"),
        ] {
            assert_eq!(reading(key).name, expected);
        }
    }

    #[test]
    fn a_power_that_is_an_output_says_so() {
        // They read as one family beside the slot and default settings that command them.
        assert_eq!(reading("ac_power").name, "AC output power");
        assert_eq!(reading("on_grid_power").name, "On-grid output power");
        assert_eq!(reading("off_grid_power").name, "Off-grid output power");
        assert_eq!(setting("default_output_power").name, "Default output power");
        assert_eq!(setting("slot1_output_power").name, "Slot 1 output power");

        // The field names stay as the specification has them: they are what the state topic carries and
        // what every capture contains.
        assert_eq!(reading("ac_power").key, "ac_power");
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
