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

use crate::model::{Raw, Register, Repeat, Scaling, Unit, Value};

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
    /// The days a schedule slot repeats on.
    Weekdays,
    /// Free text.
    Text,
}

impl Shape {
    /// The raw register value a piece of text stands for, or `None` if it is not one this shape accepts.
    ///
    /// Written once and used twice: Home Assistant sends a setting's value as text on its command topic,
    /// and the control API accepts the same text in a write body. Both then need the same answer, and the
    /// shape is the only thing that knows it.
    ///
    /// Composition only — whether the composed value is *acceptable* is the driver's domain to say, and it
    /// says so on the way to the device.
    ///
    /// A switch and a number have **no** text form: they are written as JSON numbers, and quoting one is a
    /// caller's mistake worth reporting rather than coercing, since a register that holds whole watts
    /// should not silently accept `"100"`.
    pub fn parse(self, text: &str) -> Option<u16> {
        match self {
            Self::Switch | Self::Number { .. } => None,
            Self::Text => text.trim().parse().ok(),
            Self::Choice { labels } => {
                let index = labels.iter().position(|known| *known == text.trim())?;
                u16::try_from(index).ok()
            }
            Self::TimeOfDay => {
                let (hours, minutes) = text.trim().split_once(':')?;
                let hour: u16 = hours.parse().ok()?;
                let minute: u16 = minutes.parse().ok()?;
                hour.checked_mul(256)?.checked_add(minute)
            }
            Self::Weekdays => Repeat::parse(text).map(|repeat| u16::from(repeat.mask())),
        }
    }

    /// What this shape accepts, for an error message.
    pub fn accepts(self) -> String {
        match self {
            Self::Switch => "0 or 1".to_owned(),
            Self::Number { min, max } => format!("a whole number {min}..={max}"),
            Self::Choice { labels } => format!("one of {}", labels.join(", ")),
            Self::TimeOfDay => "a time as \"HH:MM\"".to_owned(),
            Self::Weekdays => "days as \"mon,tue\", or \"daily\" for every day".to_owned(),
            Self::Text => "text".to_owned(),
        }
    }
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

    /// Another setting whose value makes this one inoperative, and at which values.
    ///
    /// A control that has no effect is worse than one that is missing: the operator changes it and nothing
    /// happens. Naming the setting responsible is what lets a surface hide or disable it instead.
    fn superseded_by(&self) -> Option<(&'static str, &'static [u16])> {
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

/// One power a device measures as a voltage and a current, both by the driver's own field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoltAmps {
    /// The voltage reading.
    pub volts: &'static str,
    /// The current reading.
    pub amps: &'static str,
}

/// A product's PV inputs, and the voltage below which an input has no panel output behind it.
///
/// Without light an MPPT input still reads a few volts of its own bias and a current offset of up to a few
/// hundred milliamps, which multiplies into watts of PV in the dark. Below `live_volts` an input is
/// counted as delivering nothing. That is a test of whether the current reading means anything, not a
/// dead band on power: a panel in any light that can deliver sits well above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strings {
    /// Each input.
    pub inputs: &'static [VoltAmps],
    /// The voltage from which an input is taken to be delivering.
    pub live_volts: u16,
}

/// A measured power, and the reading the device's controller presents in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow<M> {
    /// What it is measured from.
    pub measured: M,
    /// The reading the controller presents for the same quantity.
    pub presented: &'static str,
    /// What the presented reading is called once the measured value has taken its name.
    pub reported: &'static str,
    /// Whether the presented reading counts the other way: an output that is negative while delivering.
    pub inverted: bool,
}

/// How a product's power flows are measured, as against how its controller presents them.
///
/// A controller can report its power flows as a summary for its own app rather than as measurements:
/// balanced so the figures add up, with small values shown as zero. The measurements those figures are
/// made from are often in the same frame as voltages and currents. This says which, so a consumer can
/// publish what was measured under the name people already read, and keep the summary beside it.
///
/// Per product rather than per driver, because one protocol can serve products that measure different
/// things. A flow a product does not measure is `None`, and its presented reading stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PowerFlows {
    /// PV, summed over its inputs.
    pub pv: Option<Flow<Strings>>,
    /// The battery, positive while charging.
    pub battery: Option<Flow<VoltAmps>>,
    /// The AC output, summed over the readings that carry it, positive while delivering.
    pub ac_output: Option<Flow<&'static [&'static str]>>,
}

/// One frame's measured power flows, in watts, rounded to a tenth.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Measured {
    /// PV, as delivered by the panels.
    pub pv: Option<f64>,
    /// The battery, positive while charging.
    pub battery: Option<f64>,
    /// The AC output, positive while delivering and negative while drawing from the grid.
    pub ac_output: Option<f64>,
}

impl PowerFlows {
    /// Measure a frame, given its readings by name.
    ///
    /// A flow whose inputs are missing from the frame is `None` rather than zero: a zero would be a
    /// measurement nobody made.
    pub fn measure(&self, value: impl Fn(&str) -> Option<f64>) -> Measured {
        let watts = |pair: &VoltAmps| Some(value(pair.volts)? * value(pair.amps)?);
        let string = |strings: Strings, pair: &VoltAmps| {
            let volts = value(pair.volts)?;
            let live = volts >= f64::from(strings.live_volts);
            Some(if live { volts * value(pair.amps)? } else { 0.0 })
        };
        let pv = self.pv.and_then(|flow| {
            let strings = flow.measured;
            strings
                .inputs
                .iter()
                .map(|pair| string(strings, pair))
                .sum::<Option<f64>>()
        });
        let battery = self.battery.and_then(|flow| watts(&flow.measured));
        let ac_output = self
            .ac_output
            .and_then(|flow| flow.measured.iter().map(|name| value(name)).sum::<Option<f64>>());
        Measured {
            pv: pv.map(tenths),
            battery: battery.map(tenths),
            ac_output: ac_output.map(tenths),
        }
    }
}

impl Measured {
    /// The power the device itself consumes: what came in from the panels, less what went into the
    /// battery and out of the AC side.
    ///
    /// Only where all three are measured. The inputs come from different components and are not sampled
    /// at the same instant, so a single frame during a step can stand out; across a steady flow it is the
    /// conversion loss.
    pub fn conversion_loss(&self) -> Option<f64> {
        Some(tenths(self.pv? - self.battery? - self.ac_output?))
    }
}

/// Rounded to a tenth of a watt: finer than any of the inputs resolves once multiplied.
///
/// Adding zero turns the `-0.0` a small negative value rounds to into `0.0`, which is what it is.
fn tenths(watts: f64) -> f64 {
    (watts * 10.0).round() / 10.0 + 0.0
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

    /// How a product's power flows are measured, for a device reporting `device_type`.
    ///
    /// Nothing measured by default: the presented readings stand.
    fn power_flows(&self, _device_type: Option<&str>) -> PowerFlows {
        PowerFlows::default()
    }
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

#[cfg(test)]
mod tests {
    use super::Shape;

    /// The labels a work-mode choice offers, standing in for a driver's own.
    const MODES: &[&str] = &["load_first", "battery_first", "smart_self_use"];

    #[test]
    fn a_shape_parses_the_text_a_read_would_render() {
        assert_eq!(Shape::TimeOfDay.parse("23:59"), Some(0x173B));
        assert_eq!(Shape::Weekdays.parse("mon,wed"), Some(5));
        assert_eq!(Shape::Weekdays.parse("daily"), Some(0));
        assert_eq!(Shape::Choice { labels: MODES }.parse("battery_first"), Some(1));
    }

    #[test]
    fn a_switch_and_a_number_have_no_text_form() {
        // Deliberate: a register holding whole watts should not quietly accept `"100"`, because a quoted
        // number is a caller confusing a string for a value rather than a shorthand worth supporting.
        assert_eq!(Shape::Switch.parse("1"), None);
        assert_eq!(Shape::Number { min: 0, max: 1000 }.parse("100"), None);
    }

    #[test]
    fn text_that_names_nothing_is_refused_rather_than_coerced() {
        assert_eq!(Shape::TimeOfDay.parse("2359"), None);
        assert_eq!(Shape::Weekdays.parse("mon,funday"), None);
        assert_eq!(Shape::Choice { labels: MODES }.parse("vpp"), None);
    }

    #[test]
    fn every_shape_says_what_it_accepts() {
        for shape in [
            Shape::Switch,
            Shape::Number { min: 0, max: 10 },
            Shape::Choice { labels: MODES },
            Shape::TimeOfDay,
            Shape::Weekdays,
            Shape::Text,
        ] {
            assert!(!shape.accepts().is_empty(), "{shape:?}");
        }
    }
}
