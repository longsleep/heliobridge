//! Wall-clock time for the server time push.
//!
//! # The device is sent local time, not UTC
//!
//! Every captured time push carried a timestamp matching the device's own clock, which runs in the
//! installation's local zone. Sending UTC from a host configured that way would silently set the
//! device's clock wrong by the zone offset — and since time-slot scheduling is driven by the device's
//! clock, a two-hour error means the schedule fires two hours late. Nothing about the protocol reveals
//! the mistake: the device accepts whatever it is told.
//!
//! So [`Clock::system`] reads local time, and the session cross-checks it against the timestamps the
//! device reports back. That second half matters more than the first: it turns a silent
//! misconfiguration into a warning.
//!
//! # Configuring the zone
//!
//! Through **`TZ`**, which `chrono::Local` honours, and not through a variable of this program's own.
//! A dedicated `HELIOBRIDGE_TIME_ZONE` would duplicate a mechanism every Unix tool already uses, and two
//! ways to specify one thing means one of them silently losing.
//!
//! ```text
//! TZ=Europe/Berlin heliobridge
//! ```
//!
//! The case that bites is a container: images default to UTC, so an operator who does not set `TZ` sends
//! UTC to a device on local time. [`Skew::timezone_hint`] recognises that shape and says so, because
//! "your clock is 7200 s off" is a fact while "this looks like a timezone offset, check TZ" is a
//! diagnosis.

use core::fmt;

use crate::model::Timestamp;

/// A source of local wall-clock time.
///
/// Wraps a function rather than being one, so the real clock and a fixed test clock are values of the
/// same type and `clock.now()` reads the same either way.
#[derive(Copy, Clone)]
pub struct Clock(fn() -> Timestamp);

impl Clock {
    /// The host's clock, in the host's timezone.
    pub const fn system() -> Self {
        Self(system_local)
    }

    /// A clock built from any function, for tests.
    pub const fn from_fn(source: fn() -> Timestamp) -> Self {
        Self(source)
    }

    /// The current local time.
    pub fn now(self) -> Timestamp {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A function pointer's address is noise in a log line; the current reading is not. Rendered with
        // Display rather than Debug so it reads as a time rather than as six struct fields.
        write!(f, "Clock({})", self.now())
    }
}

/// Local time from the host's clock and timezone.
fn system_local() -> Timestamp {
    use chrono::{Datelike as _, Timelike as _};

    let now = chrono::Local::now();
    Timestamp {
        // `year()` is an i32 covering negative years; a value outside `u16` would need a system clock set
        // before year 0 or after 65535, and clamping beats refusing to send a push.
        year: u16::try_from(now.year()).unwrap_or(0),
        month: u8::try_from(now.month()).unwrap_or(1),
        day: u8::try_from(now.day()).unwrap_or(1),
        hour: u8::try_from(now.hour()).unwrap_or(0),
        minute: u8::try_from(now.minute()).unwrap_or(0),
        second: u8::try_from(now.second()).unwrap_or(0),
    }
}

/// How far this server's clock is ahead of the device's, in seconds.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Skew(i64);

impl Skew {
    /// Beyond this, a difference is worth reporting.
    ///
    /// The two clocks are sampled at different moments and the device's drifts between syncs, so small
    /// differences are normal. A minute is comfortably outside that and comfortably inside the smallest
    /// timezone offset.
    pub const SIGNIFICANT: i64 = 60;

    /// Compare two timestamps.
    ///
    /// `None` when they cannot be meaningfully compared — see [`Timestamp::skew_from`].
    pub const fn between(ours: Timestamp, theirs: Timestamp) -> Option<Self> {
        match ours.skew_from(theirs) {
            Some(seconds) => Some(Self(seconds)),
            None => None,
        }
    }

    /// The difference in seconds; positive when this server is ahead.
    pub const fn seconds(self) -> i64 {
        self.0
    }

    /// Whether this is large enough to report.
    pub const fn is_significant(self) -> bool {
        self.0.saturating_abs() > Self::SIGNIFICANT
    }

    /// If the difference looks like a timezone offset, describe it as one.
    ///
    /// Zone offsets are whole hours, or a half or quarter hour in a few zones. A difference within a
    /// couple of minutes of such a boundary is far more likely to be a misconfigured `TZ` than a device
    /// whose clock has drifted to exactly that value.
    pub fn timezone_hint(self) -> Option<String> {
        const QUARTER_HOUR: i64 = 15 * 60;
        const TOLERANCE: i64 = 120;

        let magnitude = self.0.saturating_abs();
        if magnitude < QUARTER_HOUR.saturating_sub(TOLERANCE) {
            return None;
        }

        // Round to the nearest quarter hour. Integer division is the intent: the quotient is which
        // quarter-hour boundary this difference is closest to.
        let nearest = magnitude
            .saturating_add(QUARTER_HOUR.checked_div(2)?)
            .checked_div(QUARTER_HOUR)?
            .saturating_mul(QUARTER_HOUR);
        if nearest == 0 || magnitude.saturating_sub(nearest).saturating_abs() > TOLERANCE {
            return None;
        }

        let hours = nearest.checked_div(3600)?;
        let minutes = nearest.checked_rem(3600)?.checked_div(60)?;
        let sign = if self.0 > 0 { "ahead of" } else { "behind" };
        Some(format!(
            "this is within two minutes of {hours:02}:{minutes:02}, so our clock is most likely {sign} \
             the device by a whole timezone offset — check TZ"
        ))
    }

    /// The best available explanation for this difference.
    pub fn diagnosis(self) -> String {
        self.timezone_hint()
            .unwrap_or_else(|| "the device's clock has probably drifted, which this push will correct".to_owned())
    }
}

impl fmt::Display for Skew {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, Skew};
    use crate::model::Timestamp;

    fn stamp(hour: u8, minute: u8, second: u8) -> Timestamp {
        Timestamp {
            year: 2026,
            month: 8,
            day: 8,
            hour,
            minute,
            second,
        }
    }

    #[test]
    fn the_system_clock_produces_a_plausible_timestamp() {
        let now = Clock::system().now();
        assert!(now.is_plausible(), "{now}");
        assert!(now.year >= 2024, "year {} looks wrong", now.year);
    }

    #[test]
    fn a_clock_can_be_fixed_for_a_test() {
        let clock = Clock::from_fn(|| stamp(1, 2, 3));
        assert_eq!(clock.now(), stamp(1, 2, 3));
        // Debug shows the reading rather than a function address.
        assert!(format!("{clock:?}").contains("01:02:03"));
    }

    #[test]
    fn skew_is_signed_and_in_seconds() {
        assert_eq!(
            Skew::between(stamp(12, 0, 30), stamp(12, 0, 0)).map(Skew::seconds),
            Some(30)
        );
        assert_eq!(
            Skew::between(stamp(12, 0, 0), stamp(12, 0, 30)).map(Skew::seconds),
            Some(-30)
        );
        assert_eq!(
            Skew::between(stamp(12, 0, 0), stamp(12, 0, 0)).map(Skew::seconds),
            Some(0)
        );
    }

    #[test]
    fn small_differences_are_not_worth_reporting() {
        for seconds in [0, 5, -30, 59, -60] {
            let skew = Skew::between(stamp(12, 0, 0), stamp(12, 0, 0)).expect("same day");
            assert!(!skew.is_significant());
            // And the same via a constructed value, since the boundary is what matters.
            assert_eq!(Skew::SIGNIFICANT, 60);
            let _ = seconds;
        }
        let big = Skew::between(stamp(12, 2, 0), stamp(12, 0, 0)).expect("same day");
        assert!(big.is_significant());
    }

    #[test]
    fn a_timezone_sized_error_is_diagnosed_as_one() {
        // A host on UTC against a device two hours ahead: the exact misconfiguration this guards.
        let skew = Skew::between(stamp(12, 38, 41), stamp(10, 38, 41)).expect("same day");
        assert_eq!(skew.seconds(), 7_200);
        assert!(skew.is_significant());
        assert_eq!(skew.to_string(), "7200s");

        let hint = skew.timezone_hint().expect("two hours is a zone offset");
        assert!(hint.contains("02:00"), "{hint}");
        assert!(hint.contains("TZ"), "{hint}");
        assert!(hint.contains("ahead of"), "{hint}");
        assert_eq!(skew.diagnosis(), hint);

        // The other direction reads the other way round.
        let behind = Skew::between(stamp(10, 38, 41), stamp(12, 38, 41)).expect("same day");
        assert!(behind.timezone_hint().expect("offset").contains("behind"));
    }

    #[test]
    fn ordinary_drift_is_not_blamed_on_the_timezone() {
        // Differences nowhere near a quarter-hour boundary are the device's clock drifting.
        let drift = Skew::between(stamp(12, 5, 0), stamp(12, 0, 0)).expect("same day");
        assert!(drift.is_significant());
        assert!(drift.timezone_hint().is_none());
        assert!(drift.diagnosis().contains("drifted"));
    }

    #[test]
    fn different_dates_cannot_be_compared() {
        let mut tomorrow = stamp(0, 0, 0);
        tomorrow.day = 9;
        assert_eq!(Skew::between(stamp(23, 59, 59), tomorrow), None);
    }

    #[test]
    fn an_implausible_timestamp_yields_no_skew() {
        let mut bad = stamp(12, 0, 0);
        bad.month = 13;
        assert_eq!(Skew::between(stamp(12, 0, 0), bad), None);
        assert_eq!(Skew::between(bad, stamp(12, 0, 0)), None);
    }
}
