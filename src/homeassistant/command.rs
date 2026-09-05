//! Turning what Home Assistant publishes into something the device will accept.
//!
//! A command arrives as a JSON object naming a setting and a value — `{"slot1_output_power": 100}` — which
//! is what the discovery messages tell Home Assistant to send. This module's whole job is to refuse
//! everything that is not that, and to refuse it *here* rather than on the wire.
//!
//! # The allowlist is inherited, not re-implemented
//!
//! Every accepted payload becomes a [`Command`], which can only be built from the holding register map with
//! a value inside the register's domain. There is no path from a broker message to a register the encoder
//! would refuse — the same guarantee the control socket has, because it is the same encoder.
//!
//! # What arrives is decided by the discovery message
//!
//! Each component sends its value differently, and the shapes here are the other half of what
//! [`crate::homeassistant::discovery`] published: a number for a `number`, `1` or `0` for a `switch`, the
//! chosen label for a `select`, `HH:MM` for the `text` entities standing in for a missing time component.
//! Anything else is a mistake worth naming rather than coercing.

use serde_json::Value;
use snafu::Snafu;

use crate::driver::commands::Command;
use crate::growatt::v7::encode::WritableConfig;
use crate::growatt::v7::meter;
use crate::growatt::v7::registers::{Domain, HoldingRegister, SLOT_COUNT};
use crate::homeassistant::entity::{METER_READING, WITHDRAW_METER_READING};
use crate::model::Register;

/// What a command topic is allowed to change.
///
/// Two switches rather than one, because they refuse for different reasons: one is "this bridge does not
/// write at all", the other is "this particular setting must not be reachable from here".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permitted {
    /// Whether any setting may be written.
    pub writes: bool,
    /// Whether `power_plus` may be written.
    ///
    /// Separate from [`Self::writes`] so it can be made unreachable on its own, leaving every other setting
    /// writable. Cleared, it is published as a reading *and* any command naming it is refused: a retained
    /// command on the broker, or a hand-published one, must not get through a control that was taken away.
    pub power_plus: bool,
}

impl Default for Permitted {
    fn default() -> Self {
        Self {
            writes: true,
            power_plus: true,
        }
    }
}

/// The setting whose reachability is configurable on its own.
pub const POWER_PLUS: &str = "power_plus";

impl Permitted {
    /// Whether a named setting may be written.
    pub fn allows(self, name: &str) -> bool {
        self.writes && (self.power_plus || name != POWER_PLUS)
    }
}

/// Why a command payload was refused.
#[derive(Debug, Snafu, PartialEq, Eq)]
#[snafu(visibility(pub))]
pub enum CommandError {
    /// The payload was not a JSON object.
    #[snafu(display("expected a JSON object naming a setting, like {{\"slot1_output_power\": 100}}"))]
    Malformed,

    /// The object named nothing.
    #[snafu(display("the command named no setting"))]
    Empty,

    /// No such setting.
    #[snafu(display("unknown setting {key:?}"))]
    Unknown {
        /// What was named.
        key: String,
    },

    /// The setting exists but this bridge will not write it.
    #[snafu(display("{key} is not writable from here"))]
    Refused {
        /// What was named.
        key: String,
    },

    /// The value was not of the shape this setting takes.
    #[snafu(display("{key} takes {expected}, not {got}"))]
    Shape {
        /// What was named.
        key: String,
        /// What it accepts.
        expected: String,
        /// What arrived.
        got: String,
    },

    /// The value is outside what the device accepts.
    #[snafu(display("{key}: accepts {accepted}, not {value}"))]
    Domain {
        /// What was named.
        key: String,
        /// What the register accepts.
        accepted: String,
        /// The value offered.
        value: u16,
    },
}

/// How a change is delivered, and what counts as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A setting: written, then read back, and the stored value is what gets published.
    Confirmed,
    /// An action: transmitted, with nothing to read back. The config space draws no acknowledgement and
    /// these registers hold no readable value, so "sent" is the strongest answer available.
    FireAndForget,
}

/// One setting change or action, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The field name, for logging and for the read-back that follows.
    pub key: String,
    /// The register it addresses.
    pub register: Register,
    /// The command that carries it out.
    pub command: Command,
    /// How to deliver it.
    pub delivery: Delivery,
    /// What was asked for, where the answer will not say.
    ///
    /// A setting's outcome carries the value the device stored, so nothing needs to be remembered for it.
    /// A fire-and-forget write has no such answer, and one of them — a supplied meter reading — carries a
    /// figure that matters: "a reading was sent" is not a record of what the device was told.
    pub requested: Option<String>,
}

impl Change {
    /// Every change a command payload asks for, or the first reason it cannot be honoured.
    ///
    /// All or nothing: a payload with one bad field applies none of it. A partial application would leave
    /// the device in a state nobody asked for, and the caller cannot tell which half took effect.
    ///
    /// # Errors
    ///
    /// [`CommandError`] naming the field and what was wrong with it.
    pub fn from_payload(payload: &[u8], permitted: Permitted) -> Result<Vec<Self>, CommandError> {
        let value: Value = serde_json::from_slice(payload).map_err(|_ignored| CommandError::Malformed)?;
        let object = value.as_object().ok_or(CommandError::Malformed)?;
        if object.is_empty() {
            return Err(CommandError::Empty);
        }

        object
            .iter()
            .map(|(key, value)| Self::one(key, value, permitted))
            .collect()
    }

    /// One field of a command payload.
    fn one(key: &str, value: &Value, permitted: Permitted) -> Result<Self, CommandError> {
        if let Some(action) = Self::action(key, permitted) {
            return action;
        }
        if let Some(reading) = Self::meter_reading(key, value, permitted) {
            return reading;
        }

        let register = HoldingRegister::resync_set(SLOT_COUNT)
            .into_iter()
            .find(|entry| entry.name == key)
            .ok_or_else(|| CommandError::Unknown { key: key.to_owned() })?;

        if !permitted.allows(key) {
            return Err(CommandError::Refused { key: key.to_owned() });
        }

        let raw = raw_value(&register, value).ok_or_else(|| CommandError::Shape {
            key: key.to_owned(),
            expected: describe(register.domain),
            got: rendered(value),
        })?;

        // Checked here as well as by the driver, which refuses the same value again before there are any
        // octets: a control that answers "42 is not a work mode" is more use than one that reports a
        // command it could not send. Either way the device never gets a chance to clamp it silently.
        if !register.domain.accepts(raw) {
            return Err(CommandError::Domain {
                key: key.to_owned(),
                accepted: register.domain.describe(),
                value: raw,
            });
        }

        Ok(Self {
            key: key.to_owned(),
            register: register.register,
            command: Command::Set {
                register: register.register,
                value: raw,
            },
            delivery: Delivery::Confirmed,
            requested: None,
        })
    }

    /// The supplied meter reading, if the key names one of its two controls.
    ///
    /// Not a setting and not a config action, so it resolves here rather than through either table: the
    /// registers hold a reading the device consumes, they answer no read-back, and the value expires. What
    /// arrives is therefore delivered and reported as sent.
    ///
    /// Withdrawing is its own key rather than a magic value of the reading, because `0` is a *valid*
    /// reading — the grid is balanced — and the device acts on it by holding its output. Conflating the two
    /// would make "my meter has gone" unsayable.
    fn meter_reading(key: &str, value: &Value, permitted: Permitted) -> Option<Result<Self, CommandError>> {
        let withdrawing = match key {
            METER_READING => false,
            WITHDRAW_METER_READING => true,
            _ => return None,
        };

        if !permitted.allows(key) {
            return Some(Err(CommandError::Refused { key: key.to_owned() }));
        }

        // A button carries no value worth reading; a reading is a signed number of watts.
        let watts = if withdrawing {
            0
        } else {
            match value.as_i64().and_then(|watts| i32::try_from(watts).ok()) {
                Some(watts) => watts,
                None => {
                    return Some(Err(CommandError::Shape {
                        key: key.to_owned(),
                        expected: "a signed number of watts".to_owned(),
                        got: rendered(value),
                    }));
                }
            }
        };

        Some(Ok(Self {
            key: key.to_owned(),
            register: meter::FIRST_REGISTER,
            command: Command::MeterReading {
                watts,
                valid: !withdrawing,
            },
            delivery: Delivery::FireAndForget,
            requested: Some(if withdrawing {
                "withdrawn".to_owned()
            } else {
                format!("{watts} W")
            }),
        }))
    }

    /// The config-space action a key names, if it names one.
    ///
    /// Resolved from the encoder's own list rather than a second table here, so a name can only mean what
    /// the encoder already says it means. A destructive action is refused rather than left unmatched: it is
    /// not published as an entity, and a command naming it — retained on the broker, or hand-published —
    /// must not get through a control that was never offered.
    fn action(key: &str, permitted: Permitted) -> Option<Result<Self, CommandError>> {
        let action = WritableConfig::ALL
            .into_iter()
            .find(|config| config.is_action() && config.name() == key)?;

        // Both refusals produce the same answer, because both mean the same thing to a caller: this
        // name is not a control that may be operated. Destructive actions are never offered, and refusing
        // writes withdraws the rest.
        if action.is_destructive() || !permitted.allows(key) {
            return Some(Err(CommandError::Refused { key: key.to_owned() }));
        }
        let value = action.trigger_value()?.to_owned();

        Some(Ok(Self {
            key: key.to_owned(),
            register: action.register(),
            command: Command::WriteConfig {
                register: action.register(),
                value,
            },
            delivery: Delivery::FireAndForget,
            requested: None,
        }))
    }

    /// Whether this change may have moved another register with it.
    ///
    /// `power_plus` gates the output ceiling: clearing it reduces a stored `default_output_power` with no
    /// write to that register at all. Nothing models the dependency — the affected register is read back.
    pub fn also_read(&self) -> Option<Register> {
        (self.key == POWER_PLUS).then_some(Register(322))
    }
}

/// The raw register value a JSON value means for this setting, or `None` if it is the wrong shape.
fn raw_value(register: &HoldingRegister, value: &Value) -> Option<u16> {
    match register.domain {
        // A switch's payloads are written out in its discovery message as `{"key": 1}` and `{"key": 0}`. A
        // JSON boolean is accepted too, since a hand-written command is the obvious place for one.
        Domain::Flag => match value {
            Value::Bool(flag) => Some(u16::from(*flag)),
            _ => number(value).filter(|raw| *raw <= 1),
        },
        Domain::Range { .. } => number(value),
        Domain::Enum(labels) => {
            let label = value.as_str()?;
            let index = labels.iter().position(|known| *known == label)?;
            u16::try_from(index).ok()
        }
        Domain::TimeOfDay => {
            let (hours, minutes) = value.as_str()?.split_once(':')?;
            let hour: u16 = hours.parse().ok()?;
            let minute: u16 = minutes.parse().ok()?;
            // Composed rather than validated here: the domain decides what is acceptable, and it is the
            // same check the encoder applies.
            hour.checked_mul(256)?.checked_add(minute)
        }
    }
}

/// A JSON number as a register value.
fn number(value: &Value) -> Option<u16> {
    // Integers only. A fractional value in a register that holds whole watts is a mistake in the caller,
    // and truncating it silently would store something they did not ask for.
    u16::try_from(value.as_u64()?).ok()
}

/// What a setting accepts, for an error message.
fn describe(domain: Domain) -> String {
    match domain {
        Domain::Flag => "0 or 1".to_owned(),
        Domain::Range { min, max } => format!("a whole number {min}..={max}"),
        Domain::Enum(labels) => format!("one of {}", labels.join(", ")),
        Domain::TimeOfDay => "a time as \"HH:MM\"".to_owned(),
    }
}

/// A JSON value as it would read in a message, without dumping a whole document into a log line.
fn rendered(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("{text:?}"),
        Value::Array(_) => "an array".to_owned(),
        Value::Object(_) => "an object".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, CommandError, Delivery, POWER_PLUS, Permitted, WritableConfig};
    use crate::model::Register;

    fn parse(payload: &str) -> Result<Vec<Change>, CommandError> {
        Change::from_payload(payload.as_bytes(), Permitted::default())
    }

    /// The one change a payload asks for.
    fn only(payload: &str) -> Change {
        let mut changes = parse(payload).expect("accepted");
        assert_eq!(changes.len(), 1, "{payload}");
        changes.remove(0)
    }

    #[test]
    fn each_component_sends_its_value_the_way_discovery_said_it_would() {
        // The other half of the discovery message. A number for a number, 1 or 0 for a switch, the chosen
        // label for a select, HH:MM for the text entities standing in for a missing time component.
        assert_eq!(only(r#"{"slot1_output_power": 100}"#).register, Register(257));
        assert_eq!(only(r#"{"grid_power_allowed": 1}"#).register, Register(326));
        assert_eq!(only(r#"{"slot1_work_mode": "smart_self_use"}"#).register, Register(256));
        assert_eq!(only(r#"{"slot1_start_time": "23:59"}"#).register, Register(254));
    }

    #[test]
    fn a_switch_takes_a_boolean_as_well_as_a_flag() {
        // Home Assistant sends the payloads discovery gave it, which are numbers. A hand-written command is
        // the obvious place for a boolean.
        for payload in [r#"{"always_on": 1}"#, r#"{"always_on": true}"#] {
            assert_eq!(only(payload).register, Register(304), "{payload}");
        }
    }

    #[test]
    fn a_time_of_day_is_encoded_as_the_device_stores_it() {
        // HH << 8 | MM, which is what the register holds and what the decoder renders back.
        let change = only(r#"{"slot1_end_time": "23:59"}"#);
        assert_eq!(change.register, Register(255));
        // 23 << 8 | 59
        assert!(format!("{:?}", change.command).contains("5947"), "{:?}", change.command);
    }

    #[test]
    fn a_value_of_the_wrong_shape_is_named_rather_than_coerced() {
        // Each of these is a caller mistake, and each would otherwise store something nobody asked for.
        for payload in [
            r#"{"slot1_output_power": "100"}"#,
            r#"{"slot1_output_power": 100.5}"#,
            r#"{"slot1_output_power": -5}"#,
            r#"{"slot1_work_mode": 2}"#,
            r#"{"slot1_work_mode": "nonsense"}"#,
            r#"{"slot1_start_time": "2359"}"#,
            r#"{"slot1_start_time": 1439}"#,
            r#"{"always_on": 2}"#,
            r#"{"always_on": null}"#,
        ] {
            let error = parse(payload).expect_err(payload);
            assert!(
                matches!(error, CommandError::Shape { .. } | CommandError::Domain { .. }),
                "{payload} gave {error:?}"
            );
        }
    }

    #[test]
    fn a_value_outside_the_registers_domain_is_refused_rather_than_clamped() {
        // The device clamps silently. Refusing is the more useful outcome, and it is the encoder's decision
        // rather than a second copy of the range.
        let error = parse(r#"{"charge_limit_upper": 10}"#).expect_err("70..=100");
        assert!(matches!(error, CommandError::Domain { .. }), "{error:?}");
    }

    #[test]
    fn what_is_not_a_command_is_refused_with_a_reason() {
        assert_eq!(parse("not json").expect_err("malformed"), CommandError::Malformed);
        assert_eq!(parse("[1,2]").expect_err("not an object"), CommandError::Malformed);
        assert_eq!(parse("{}").expect_err("empty"), CommandError::Empty);
        assert!(matches!(
            parse(r#"{"nonsense": 1}"#).expect_err("unknown"),
            CommandError::Unknown { .. }
        ));
        // A telemetry field is not a setting, however real it looks.
        assert!(matches!(
            parse(r#"{"ac_power": 100}"#).expect_err("read-only"),
            CommandError::Unknown { .. }
        ));
    }

    #[test]
    fn one_bad_field_applies_none_of_the_payload() {
        // A partial application would leave the device in a state nobody asked for, and the caller cannot
        // tell which half took effect.
        let error = parse(r#"{"always_on": 1, "slot1_work_mode": "nonsense"}"#).expect_err("refused");
        assert!(matches!(error, CommandError::Shape { .. }), "{error:?}");
    }

    #[test]
    fn refusing_writes_refuses_every_setting() {
        let permitted = Permitted {
            writes: false,
            ..Permitted::default()
        };
        for payload in [r#"{"always_on": 1}"#, r#"{"slot1_output_power": 100}"#] {
            let error = Change::from_payload(payload.as_bytes(), permitted).expect_err(payload);
            assert!(matches!(error, CommandError::Refused { .. }), "{error:?}");
        }
    }

    #[test]
    fn power_plus_can_be_made_unreachable_on_its_own() {
        // Not offering the control is not enough: a retained command on the broker, or a hand-published one,
        // must not get through a control that was taken away.
        let permitted = Permitted {
            power_plus: false,
            ..Permitted::default()
        };
        let error = Change::from_payload(br#"{"power_plus": 1}"#, permitted).expect_err("refused");
        assert!(matches!(error, CommandError::Refused { .. }), "{error:?}");
        // Turning it *off* is refused too: the register is unreachable, not one-directional, so the device
        // keeps whatever it was set to elsewhere.
        assert!(
            Change::from_payload(br#"{"power_plus": 0}"#, permitted).is_err(),
            "the register is unreachable, in either direction"
        );

        // Everything else still works, and Power+ still works when it is allowed.
        assert!(Change::from_payload(br#"{"always_on": 1}"#, permitted).is_ok());
        assert!(Change::from_payload(br#"{"power_plus": 1}"#, Permitted::default()).is_ok());
    }

    #[test]
    fn a_gating_flag_names_the_register_to_read_back_with_it() {
        // Clearing power_plus reduces a stored output power with no write to it. Nothing models that; the
        // affected register is read again.
        assert_eq!(only(r#"{"power_plus": 0}"#).also_read(), Some(Register(322)));
        assert_eq!(only(r#"{"always_on": 1}"#).also_read(), None);
        assert_eq!(POWER_PLUS, "power_plus");
    }

    #[test]
    fn a_restart_is_accepted_and_sent_without_a_read_back() {
        let changes = Change::from_payload(br#"{"restart": 1}"#, Permitted::default()).expect("accepted");
        let change = changes.first().expect("one change");
        assert_eq!(change.key, "restart");
        assert_eq!(change.register.number(), 32);
        // Fire-and-forget: the config space acknowledges nothing, so waiting for a read-back would report a
        // working restart as a failure to confirm.
        assert_eq!(change.delivery, Delivery::FireAndForget);
    }

    #[test]
    fn a_factory_reset_is_refused_however_it_arrives() {
        // It is not published as an entity, so nothing in Home Assistant offers it — but a retained command
        // on the broker, a hand-published one, or a later catalogue change must not get through. What it
        // costs is a device off the network until somebody re-provisions it over Bluetooth, in person.
        let refused = Change::from_payload(br#"{"factory_reset": 1}"#, Permitted::default());
        assert_eq!(
            refused,
            Err(CommandError::Refused {
                key: "factory_reset".to_owned()
            })
        );
    }

    #[test]
    fn refusing_writes_refuses_a_restart_too() {
        let refused = Change::from_payload(
            br#"{"restart": 1}"#,
            Permitted {
                writes: false,
                ..Permitted::default()
            },
        );
        assert!(refused.is_err(), "a restart got through with writes refused");
    }

    #[test]
    fn a_supplied_reading_carries_its_value_for_the_log() {
        // These registers answer no read-back, so the log line is the only record of what the device was
        // told. Without this the log says a reading was sent and not which one.
        let changes =
            Change::from_payload(br#"{"supplied_meter_reading": 250}"#, Permitted::default()).expect("accepted");
        assert_eq!(changes[0].requested.as_deref(), Some("250 W"));
        assert_eq!(changes[0].delivery, Delivery::FireAndForget);

        let export =
            Change::from_payload(br#"{"supplied_meter_reading": -400}"#, Permitted::default()).expect("accepted");
        assert_eq!(export[0].requested.as_deref(), Some("-400 W"));

        let withdraw =
            Change::from_payload(br#"{"withdraw_meter_reading": 1}"#, Permitted::default()).expect("accepted");
        assert_eq!(withdraw[0].requested.as_deref(), Some("withdrawn"));
    }

    #[test]
    fn a_setting_needs_no_remembered_value_because_the_read_back_carries_it() {
        let changes = Change::from_payload(br#"{"slot1_output_power": 300}"#, Permitted::default()).expect("accepted");
        assert_eq!(changes[0].requested, None);
        assert_eq!(changes[0].delivery, Delivery::Confirmed);
    }

    #[test]
    fn every_writable_entity_can_be_commanded_by_the_name_it_publishes() {
        // The two halves must agree: an entity Home Assistant can change has to be a name this accepts,
        // or the command silently goes nowhere.
        let catalogue = crate::homeassistant::entity::Catalogue {
            slots: 9,
            ..crate::homeassistant::entity::Catalogue::default()
        };
        for entity in catalogue.entities().iter().filter(|entity| entity.is_writable()) {
            // Either kind: a holding-register setting, or a config-space action. Both are commandable and
            // they resolve by different routes, so checking only the first would fail a published button.
            let setting = crate::growatt::v7::registers::HoldingRegister::resync_set(9)
                .into_iter()
                .any(|entry| entry.name == entity.key);
            let action = WritableConfig::ALL
                .into_iter()
                .any(|config| config.is_action() && !config.is_destructive() && config.name() == entity.key);
            // And the supplied meter reading, which is neither: a data channel with no read-back.
            let reading = entity.key == crate::homeassistant::entity::METER_READING
                || entity.key == crate::homeassistant::entity::WITHDRAW_METER_READING;
            assert!(
                setting || action || reading,
                "{} is writable but not commandable",
                entity.key
            );
        }
    }
}
