//! The register maps, answered as the server's questions.
//!
//! Nothing is restated here: the maps stay in [`super::v7::registers`] with their confidence markers,
//! their supersessions and their scaling, and this module only says which of the seam's questions each
//! field answers. A `Domain::Range` becomes a number with bounds, an `Enum` becomes a choice, and what has
//! no counterpart on the other side — how sure anybody is that register 89 is a temperature — stays here,
//! unasked and unlost.

use crate::driver::catalogue::{
    ConfigField as ConfigFieldInfo, Flow, Measurement, PowerFlows, Setting, Shape, Strings, VoltAmps,
};
use crate::growatt::v7::encode::WritableConfig;
use crate::growatt::v7::registers::{
    CONFIG_REGISTERS, ConfigRegister, Domain, HoldingRegister, INPUT_REGISTERS, InputRegister, Kind, Role,
};
use crate::model::{Confidence, Raw, Register, Scaling, Unit, Value};

impl Setting for HoldingRegister {
    fn register(&self) -> Register {
        self.register
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn shape(&self) -> Shape {
        match self.domain {
            Domain::Range { min, max } => Shape::Number { min, max },
            Domain::Flag => Shape::Switch,
            Domain::TimeOfDay => Shape::TimeOfDay,
            Domain::Enum(labels) => Shape::Choice { labels },
            Domain::Weekdays => Shape::Weekdays,
        }
    }

    fn unit(&self) -> Unit {
        self.unit
    }

    fn decode(&self, raw: Raw) -> Value {
        Self::decode(self, raw)
    }

    fn accepts(&self, value: u16) -> bool {
        self.domain.accepts(value)
    }

    fn accepted(&self) -> String {
        self.domain.describe()
    }

    fn superseded_by(&self) -> Option<(&'static str, &'static [u16])> {
        self.superseded_by.map(|by| (by.setting, by.when))
    }
}

impl Measurement for InputRegister {
    fn register(&self) -> Register {
        self.register
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn shape(&self) -> Shape {
        match self.kind {
            // Bounds a setting has and a reading does not: nothing constrains what the device reports.
            Kind::Int | Kind::Float | Kind::Float32 => Shape::Number { min: 0, max: u16::MAX },
            Kind::Enum(labels) => Shape::Choice { labels },
            Kind::Text { .. } => Shape::Text,
        }
    }

    fn unit(&self) -> Unit {
        self.unit
    }

    fn scaling(&self) -> Scaling {
        self.scaling
    }

    fn is_unknown(&self) -> bool {
        Self::is_unknown(self)
    }

    fn gated_by(&self) -> Option<&'static str> {
        self.gated_by
    }
}

/// One configuration field, from either of the two things Growatt knows about one.
///
/// The map says what a field *is* — its name and what it is for — and the allowlist says whether this
/// implementation will write it and what happens if it does. A caller wants both at once, so they travel
/// together; either half may be absent, since the device reports fields nobody has named and the allowlist
/// includes the clock, which no report carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigField {
    register: Register,
    documented: Option<&'static ConfigRegister>,
    writable: Option<WritableConfig>,
}

impl ConfigField {
    /// Everything known about one config register, or `None` if neither half names it.
    pub fn lookup(register: Register) -> Option<Self> {
        let field = Self {
            register,
            documented: ConfigRegister::lookup(register),
            writable: WritableConfig::ALL
                .into_iter()
                .find(|entry| entry.register() == register),
        };
        (field.documented.is_some() || field.writable.is_some()).then_some(field)
    }

    /// The same, by name. The two halves agree on names, so either may match.
    pub fn lookup_name(name: &str) -> Option<Self> {
        if let Some(entry) = ConfigRegister::lookup_name(name) {
            return Self::lookup(entry.register);
        }
        WritableConfig::lookup(name).and_then(|entry| Self::lookup(entry.register()))
    }

    /// The allowlist, as fields.
    pub fn writable() -> Vec<Self> {
        WritableConfig::ALL
            .into_iter()
            .filter_map(|entry| Self::lookup(entry.register()))
            .collect()
    }
}

impl ConfigFieldInfo for ConfigField {
    fn register(&self) -> Register {
        self.register
    }

    fn name(&self) -> &'static str {
        self.documented
            .map(|entry| entry.name)
            .or_else(|| self.writable.map(WritableConfig::name))
            .unwrap_or("unknown")
    }

    fn role(&self) -> Option<&'static str> {
        self.documented.map(|entry| Role::as_str(entry.role))
    }

    fn action(&self) -> Option<&'static str> {
        self.writable.and_then(WritableConfig::trigger_value)
    }

    fn is_retarget(&self) -> bool {
        self.writable.is_some_and(WritableConfig::is_retarget)
    }

    fn is_destructive(&self) -> bool {
        self.writable.is_some_and(WritableConfig::is_destructive)
    }
}

/// A stand-in for a register the maps do not document.
///
/// Reading one is harmless and the value is still a fact; it simply has no name or bounds to render it
/// with, so it gets the widest domain and a name that says as much.
pub fn placeholder(register: Register) -> HoldingRegister {
    HoldingRegister::range(
        register.number(),
        "unknown",
        0,
        u16::MAX,
        Unit::None,
        Confidence::Inferred,
    )
}

/// Every reading the map documents.
///
/// Copies rather than references: an entry is a handful of words, the map is read once per announcement,
/// and a `'static` reference would make the seam's associated type a reference type for no gain.
pub fn measurements() -> Vec<InputRegister> {
    INPUT_REGISTERS.to_vec()
}

/// Every configuration register the map documents.
pub fn config_fields() -> Vec<ConfigField> {
    CONFIG_REGISTERS
        .iter()
        .filter_map(|entry| ConfigField::lookup(entry.register))
        .collect()
}

/// One PV string, as the MPPT measures it.
const fn string(volts: &'static str, amps: &'static str) -> VoltAmps {
    VoltAmps { volts, amps }
}

/// The NEXA 2000's four PV inputs.
const NEXA_STRINGS: &[VoltAmps] = &[
    string("pv1_voltage", "pv1_current"),
    string("pv2_voltage", "pv2_current"),
    string("pv3_voltage", "pv3_current"),
    string("pv4_voltage", "pv4_current"),
];

/// The NOAH 2000's two PV inputs, at the same registers as the NEXA's first two.
const NOAH_STRINGS: &[VoltAmps] = &[
    string("pv1_voltage", "pv1_current"),
    string("pv2_voltage", "pv2_current"),
];

/// The voltage from which a string is taken to be delivering.
///
/// In the dark the MPPT inputs read 6–7 V with currents up to 0.34 A, about 3 W across the four that no
/// panel produced. Across three months of one unit's frames, strings under 10 V never exceeded 2.7 W and
/// real production starts in the 20s.
const LIVE_VOLTS: u16 = 10;

/// PV, measured on a product's strings rather than taken from register 7.
///
/// The power controller builds register 7 for its app (F188): from the MPPT's own total, zeroed below 25 W
/// and whenever its output rule fires. The strings carry what the panels deliver, from a string's first watt.
const fn pv(inputs: &'static [VoltAmps]) -> Flow<Strings> {
    Flow {
        measured: Strings {
            inputs,
            live_volts: LIVE_VOLTS,
        },
        presented: "pv_power_total",
        reported: "pv_power_total_reported",
        inverted: false,
    }
}

/// What the NEXA 2000 measures: every flow.
///
/// The battery is the BMS's current times the inverter's input voltage, registers 118 and 112 — where register
/// 11 is PV − output, counting the conversion loss as charge and zeroed whenever the BMS sees under 24 W or
/// disagrees on direction. Register 112 floats while the battery is idle (F191), but it is the pack voltage
/// whenever a real current flows — no frame carrying 0.5 A or more strayed from the cells — and an idle
/// battery's current is zero, so the product stays right. The AC output is the inverter's own figure,
/// registers 116 and 111 at a tenth of a watt, where register 5 reads zero below 20 W.
pub const NEXA_FLOWS: PowerFlows = PowerFlows {
    pv: Some(pv(NEXA_STRINGS)),
    battery: Some(Flow {
        measured: VoltAmps {
            volts: "inverter_input_voltage",
            amps: "battery_current",
        },
        presented: "battery_charge_power",
        reported: "battery_charge_power_reported",
        inverted: false,
    }),
    ac_output: Some(Flow {
        measured: &["on_grid_power", "off_grid_power"],
        presented: "ac_power",
        reported: "ac_power_reported",
        inverted: true,
    }),
};

/// What the NOAH 2000 measures in its telemetry: PV alone.
///
/// Its two strings are measured as on the NEXA. It carries neither a pack voltage nor a battery current —
/// registers 112 and 118 read zero — so register 11, which on the NOAH is mostly the BMS's own reading,
/// stands. It has no AC output at all.
pub const NOAH_FLOWS: PowerFlows = PowerFlows {
    pv: Some(pv(NOAH_STRINGS)),
    battery: None,
    ac_output: None,
};
