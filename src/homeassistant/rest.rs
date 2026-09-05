//! Whether the pack is resting, and what its voltage says when it is.
//!
//! # Why this exists
//!
//! The pack's state of charge is a coulomb count, and a coulomb count drifts. This one drifted far enough
//! to read 5 % while the cells rested in the middle of their plateau, and the correction — a snap to
//! 100 % at the next full charge — also rewrote the pack's health estimate to a figure a quarter below
//! what the pack measurably delivers. Both errors were invisible while they accumulated.
//!
//! **Resting cell voltage is the only independent read on state of charge**, so it is the only thing that
//! catches the drift early. It is worth publishing for the same reason a clock comparison is: not because
//! it is precise, but because it disagrees with the other measurement when something has gone wrong.
//!
//! # What makes a sample a resting sample
//!
//! Cell voltage under load says more about internal resistance than about charge, and a cell relaxes over
//! minutes rather than seconds. So a sample counts only after the battery has been at rest — no charge,
//! no discharge — continuously for [`SETTLE`]. Anything shorter reads the recovery curve.
//!
//! # Where the thresholds come from, and what they cannot do
//!
//! Lithium iron phosphate has a flat plateau: from roughly 20 % to 80 % the resting voltage barely moves,
//! which is exactly why the drift went unnoticed. So the plateau is left alone. What the chemistry does
//! say is at the knees — a cell resting below [`LOW_KNEE`] is genuinely near empty, and one resting above
//! [`HIGH_PLATEAU`] is not near empty whatever a percentage claims. The check fires only there, and says
//! nothing in between, because a verdict the chemistry cannot support would be worse than no verdict.

use crate::control::TelemetryView;

use core::time::Duration;
use tokio::time::Instant;

/// How long the battery must have been at rest before its cell voltage means anything.
///
/// Five minutes. A cell recovering from a few hundred watts is still climbing after one.
pub const SETTLE: Duration = Duration::from_mins(5);

/// Above this current, in watts either way, the pack is not resting.
///
/// Not zero: the reported figure sits a watt or two off zero with nothing happening, and requiring an
/// exact zero would mean never taking a sample.
pub const IDLE_WATTS: f64 = 5.0;

/// Below this resting cell voltage the pack really is near empty, in millivolts.
///
/// The bottom knee. A pack that reads a low percentage *and* rests here is telling the truth.
pub const LOW_KNEE: u16 = 3200;

/// Above this resting cell voltage the pack is not near empty, in millivolts.
///
/// The middle of the plateau. A pack resting here while reporting single digits has a drifted counter:
/// this is the reading that caught it last time, at 3298 mV against a reported 5 %.
pub const HIGH_PLATEAU: u16 = 3250;

/// A state of charge this low is only credible if the cells rest at the bottom knee.
pub const CLAIMS_EMPTY: f64 = 10.0;

/// A state of charge this high is only credible if the cells rest above the bottom knee.
pub const CLAIMS_FULL: f64 = 50.0;

/// What a resting pack was measured to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resting {
    /// The lowest cell's resting voltage, in millivolts. The lowest rather than the highest because it is
    /// the one that decides when discharging stops.
    pub millivolts: u16,
    /// Whether the reported state of charge is credible against that voltage.
    ///
    /// `None` on the plateau, where the chemistry supports no verdict either way.
    pub credible: Option<bool>,
}

/// Tracks how long the pack has been at rest, and the last sample taken while it was.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestWatch {
    since: Option<Instant>,
    sample: Option<Resting>,
}

impl RestWatch {
    /// Observe one telemetry frame, returning the current resting sample if there is one.
    ///
    /// The sample survives the pack waking up: a voltage measured at rest half an hour ago is still the
    /// best independent read available, and dropping it the moment a load appears would leave the
    /// diagnostic empty almost always. It is replaced by the next resting measurement.
    pub fn observe(&mut self, view: &TelemetryView, now: Instant) -> Option<Resting> {
        let reading = |name: &str| {
            view.readings
                .iter()
                .find(|reading| reading.name == name)
                .map(|reading| reading.raw)
        };
        let watts = view
            .readings
            .iter()
            .find(|reading| reading.name == "battery_charge_power")
            .and_then(|reading| reading.value.parse::<f64>().ok());

        match watts {
            Some(watts) if watts.abs() <= IDLE_WATTS => {
                let since = *self.since.get_or_insert(now);
                if let (Some(low), Some(soc)) = (reading("battery_cell_voltage_min"), reading("battery_soc_total"))
                    && now.saturating_duration_since(since) >= SETTLE
                {
                    self.sample = Some(Resting {
                        millivolts: low,
                        credible: credible(low, f64::from(soc)),
                    });
                }
            }
            // Charging, discharging, or a frame that did not carry the figure: the clock restarts, because
            // a pack that moved is a pack whose voltage is recovering again.
            _ => self.since = None,
        }
        self.sample
    }
}

/// Whether a reported state of charge is credible against a resting cell voltage.
///
/// `None` wherever the plateau makes the question unanswerable, which is most of the range.
fn credible(millivolts: u16, soc: f64) -> Option<bool> {
    if soc <= CLAIMS_EMPTY {
        // Claims empty: believable at the knee, not believable on the plateau.
        return Some(millivolts <= LOW_KNEE);
    }
    if soc >= CLAIMS_FULL && millivolts <= LOW_KNEE {
        // Claims half full or more while resting at the bottom knee. The counter is reading high.
        return Some(false);
    }
    if millivolts >= HIGH_PLATEAU {
        // On or above the plateau with a percentage that does not claim empty: nothing to object to.
        return Some(true);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{CLAIMS_EMPTY, HIGH_PLATEAU, LOW_KNEE, credible};

    #[test]
    fn a_low_percentage_is_only_credible_at_the_knee() {
        // The case this exists for: 3298 mV at a reported 5 % is the drift that produced a bogus health
        // figure, and it must read as not credible.
        assert_eq!(credible(3298, 5.0), Some(false));
        assert_eq!(credible(3181, 5.0), Some(true));
        assert_eq!(credible(LOW_KNEE, CLAIMS_EMPTY), Some(true));
    }

    #[test]
    fn a_high_percentage_resting_at_the_bottom_is_not_credible() {
        assert_eq!(credible(3150, 60.0), Some(false));
    }

    #[test]
    fn the_plateau_supports_no_verdict() {
        // Between the knee and the plateau, with a percentage that claims neither extreme, the chemistry
        // says nothing — and saying nothing is the point.
        assert_eq!(credible(3220, 40.0), None);
        assert_eq!(credible(HIGH_PLATEAU, 40.0), Some(true));
    }
}
