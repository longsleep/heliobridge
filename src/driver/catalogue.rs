//! What a device holds and reports: its settings, its readings, its own configuration.
//!
//! # The entries stay the driver's
//!
//! A catalogue's entries are associated types, not a shape agreed here. A driver's register table knows
//! things no seam should have to model — that one setting supersedes another, that a value is scaled by
//! ten, how sure anybody is that a field means what it is called — and flattening that into a common
//! struct would lose the parts that do not fit and invent parts that do not exist. So the entries stay
//! whatever the driver already has, and this module says only which questions can be asked of them.
//!
//! # One thing does have to be shared
//!
//! [`Shape`] is the exception, and it is here because of a question the server genuinely has to answer:
//! what *control* does a setting deserve. A switch, a number with bounds, a choice between named
//! alternatives. That is the server's question — Home Assistant asks it of every setting — and no accessor
//! chain answers it exhaustively enough to be safe. Note what it is not: it is not the driver's model of
//! the register. Growatt's own domain, kind, scaling and supersession stay in Growatt, and a driver maps
//! its entries onto these five answers.
//!
//! # Static, because a catalogue is a fact about a model
//!
//! Names are `&'static str`: a catalogue describes a product family, not one device, and every
//! implementation this design anticipates has it compiled in.

use core::fmt;

use crate::model::{Raw, Register, Scaling, Unit, Value};

use super::wire::Wire;

/// What kind of control a setting deserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// On or off.
    Switch,
    /// A number, with bounds where the driver knows them.
    Number {
        /// Smallest accepted value, unscaled.
        min: u16,
        /// Largest accepted value, unscaled.
        max: u16,
    },
    /// One of a fixed set of named alternatives, indexed from zero.
    Choice {
        /// The names, in value order.
        labels: &'static [&'static str],
    },
    /// A time of day.
    TimeOfDay,
    /// Free text.
    Text,
}

/// One setting a device holds and can be asked to change.
pub trait Setting: fmt::Debug + Clone + Send + Sync + 'static {
    /// Where the device keeps it.
    fn register(&self) -> Register;

    /// What the driver calls it. Stable: it is a key in published topics and an API path.
    fn name(&self) -> &'static str;

    /// What control it deserves.
    fn shape(&self) -> Shape;

    /// What its values are measured in.
    fn unit(&self) -> Unit;

    /// Render a stored value.
    fn decode(&self, raw: Raw) -> Value;

    /// Whether the device would accept `value`.
    fn accepts(&self, value: u16) -> bool;

    /// What it does accept, for an error message a person reads.
    fn accepted(&self) -> String;

    /// Another setting whose value makes this one inoperative, and at which value.
    ///
    /// A control that has no effect is worse than one that is missing: the operator changes it and nothing
    /// happens. Naming the setting responsible is what lets a surface hide or disable it instead.
    fn superseded_by(&self) -> Option<(&'static str, u16)> {
        None
    }
}

/// One value a device reports of its own accord.
pub trait Measurement: fmt::Debug + Clone + Send + Sync + 'static {
    /// Where it comes from.
    fn register(&self) -> Register;

    /// What the driver calls it.
    fn name(&self) -> &'static str;

    /// What kind of value it is.
    fn shape(&self) -> Shape;

    /// What it is measured in.
    fn unit(&self) -> Unit;

    /// How the raw value becomes the reported one, which is also what says how many decimals it has.
    fn scaling(&self) -> Scaling;

    /// Whether nobody has established what this means yet.
    ///
    /// Such a reading is decoded and kept — that is how the next one gets identified — but publishing it
    /// as though it meant something would put noise on a dashboard.
    fn is_unknown(&self) -> bool;

    /// A reading that must be present for this one to mean anything.
    fn gated_by(&self) -> Option<&'static str> {
        None
    }
}

/// One field of the device's own configuration, as opposed to its settings.
///
/// Separate because it behaves differently: text rather than numbers, no acknowledgement, and among them
/// the two or three fields that can put a device beyond reach.
pub trait ConfigField: fmt::Debug + Clone + Send + Sync + 'static {
    /// Where the device keeps it.
    fn register(&self) -> Register;

    /// What the driver calls it.
    fn name(&self) -> &'static str;

    /// What the field is for, in the driver's own words, for display.
    fn role(&self) -> Option<&'static str> {
        None
    }

    /// Whether this is a thing to *do* rather than a value to hold, and the value that does it.
    fn action(&self) -> Option<&'static str> {
        None
    }

    /// Whether writing this moves the device to a different server.
    ///
    /// A method rather than a note, because a caller must be able to refuse the whole class without
    /// enumerating it: the failure mode is a device that never comes back.
    fn is_retarget(&self) -> bool {
        false
    }

    /// Whether carrying it out costs the operator something they must undo in person.
    fn is_destructive(&self) -> bool {
        false
    }
}

/// What a device holds, reports and can be configured with.
pub trait Catalogue: Wire {
    /// One setting, in the driver's own type.
    type Setting: Setting;
    /// One reading, likewise.
    type Measurement: Measurement;
    /// One configuration field, likewise.
    type ConfigField: ConfigField;

    /// Every setting worth reading back and publishing, with `slots` of the schedule exposed.
    fn settings(&self, slots: u16) -> Vec<Self::Setting>;

    /// One setting by register, or `None` if the driver does not document it.
    fn setting(&self, register: Register) -> Option<Self::Setting>;

    /// One setting by name.
    fn setting_named(&self, name: &str) -> Option<Self::Setting>;

    /// One setting by register, documented or not.
    ///
    /// An undocumented register is still readable and a value is still a fact; it simply has no name or
    /// bounds to render it with. A driver returns a placeholder rather than nothing, so that a caller
    /// reading a register nobody has identified does not have to invent one.
    fn describe(&self, register: Register) -> Self::Setting;

    /// The settings of one schedule slot, or `None` if the device has no such slot.
    fn slot(&self, slot: u16) -> Option<Vec<Self::Setting>>;

    /// How many schedule slots the device has.
    fn slots(&self) -> u16;

    /// Every reading the device is known to report.
    fn measurements(&self) -> Vec<Self::Measurement>;

    /// One configuration field by register.
    fn config(&self, register: Register) -> Option<Self::ConfigField>;

    /// One configuration field by name.
    fn config_named(&self, name: &str) -> Option<Self::ConfigField>;

    /// The last configuration register worth asking for, the first being zero.
    fn config_last(&self) -> Register;

    /// Every configuration field this driver will write.
    ///
    /// The allowlist, in full, so a surface can offer exactly what exists and refuse the rest by omission.
    fn writable_config(&self) -> Vec<Self::ConfigField>;
}

/// A catalogue entry for a driver that documents nothing.
///
/// Not a fallback for a real driver — that is what [`Catalogue::describe`] is for, and a driver knows
/// better what an undocumented register of its own looks like. This exists so that a driver with no
/// catalogue at all still satisfies the seam, which is what makes the null driver useful in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undocumented(pub Register);

impl Setting for Undocumented {
    fn register(&self) -> Register {
        self.0
    }

    fn name(&self) -> &'static str {
        "unknown"
    }

    fn shape(&self) -> Shape {
        Shape::Number { min: 0, max: u16::MAX }
    }

    fn unit(&self) -> Unit {
        Unit::None
    }

    fn decode(&self, raw: Raw) -> Value {
        Value::Int(raw.get())
    }

    fn accepts(&self, _value: u16) -> bool {
        false
    }

    fn accepted(&self) -> String {
        "nothing: this driver documents no settings".to_owned()
    }
}

impl Measurement for Undocumented {
    fn register(&self) -> Register {
        self.0
    }

    fn name(&self) -> &'static str {
        "unknown"
    }

    fn shape(&self) -> Shape {
        Shape::Text
    }

    fn unit(&self) -> Unit {
        Unit::None
    }

    fn scaling(&self) -> Scaling {
        Scaling::UNIT
    }

    fn is_unknown(&self) -> bool {
        true
    }
}

impl ConfigField for Undocumented {
    fn register(&self) -> Register {
        self.0
    }

    fn name(&self) -> &'static str {
        "unknown"
    }
}
