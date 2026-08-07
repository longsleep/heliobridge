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
//! So [`system_local`] reads local time, and the session cross-checks it against the timestamps the
//! device reports back. That second half matters more than the first: it turns a silent
//! misconfiguration into a warning.
//!
//! # Configuring the zone
//!
//! Through **`TZ`**, which `chrono::Local` honours, and not through a variable of this program's own.
//! A dedicated `HELIOBRIDGE_TIME_ZONE` would duplicate a mechanism every Unix tool already uses, and
//! two ways to specify one thing means one of them silently losing.
//!
//! ```text
//! TZ=Europe/Berlin heliobridge
//! ```
//!
//! The case that bites is a container: images default to UTC, so an operator who does not set `TZ`
//! sends UTC to a device on local time. [`timezone_error_hint`] recognises that shape — a skew close to
//! a whole number of hours — and says so, because "your clock is 7200 s off" is a fact while "this looks
//! like a timezone offset, check TZ" is a diagnosis.

use crate::model::Timestamp;

/// A source of local wall-clock time.
///
/// A plain function pointer rather than a trait: there is one real implementation and tests need a
/// fixed clock, which is exactly what a function pointer gives without a trait object or a generic
/// parameter threaded through the session type.
pub type Clock = fn() -> Timestamp;

/// Local time from the host's clock and timezone.
///
/// # Panics
///
/// Never. The conversions used are total for any value `chrono` can produce.
pub fn system_local() -> Timestamp {
    use chrono::{Datelike as _, Timelike as _};

    let now = chrono::Local::now();
    Timestamp {
        // `year()` is an i32 covering negative years; a value outside `u16` would need a system clock
        // set before year 0 or after 65535, and clamping is more useful than refusing to send a push.
        year: u16::try_from(now.year()).unwrap_or(0),
        month: u8::try_from(now.month()).unwrap_or(1),
        day: u8::try_from(now.day()).unwrap_or(1),
        hour: u8::try_from(now.hour()).unwrap_or(0),
        minute: u8::try_from(now.minute()).unwrap_or(0),
        second: u8::try_from(now.second()).unwrap_or(0),
    }
}

/// Difference in seconds between two timestamps, treating both as the same timezone.
///
/// Deliberately crude: a calendar-correct difference would need a date library on both sides, and the
/// only question being asked is "are these roughly the same moment". Returns `None` when either side is
/// implausible, or when the two fall on different dates — in which case the answer is "very far apart"
/// rather than a number worth computing.
pub fn skew_seconds(ours: Timestamp, theirs: Timestamp) -> Option<i64> {
    if !ours.is_plausible() || !theirs.is_plausible() {
        return None;
    }
    if (ours.year, ours.month, ours.day) != (theirs.year, theirs.month, theirs.day) {
        return None;
    }
    // Both sides are plausible, so each fits comfortably inside a day's worth of seconds; the checked
    // forms document that rather than relying on the reader to confirm it.
    let seconds = |stamp: Timestamp| -> i64 {
        i64::from(stamp.hour)
            .saturating_mul(3600)
            .saturating_add(i64::from(stamp.minute).saturating_mul(60))
            .saturating_add(i64::from(stamp.second))
    };
    Some(seconds(ours).saturating_sub(seconds(theirs)))
}

/// How far apart two clocks may be before it is worth complaining, in seconds.
///
/// The device's timestamps and ours are sampled at different moments and its clock drifts between
/// syncs, so small differences are normal. A minute is comfortably outside that and comfortably inside
/// the smallest timezone offset.
pub const SKEW_WARN_SECONDS: i64 = 60;

/// If a skew looks like a timezone offset, describe it as one.
///
/// Zone offsets are whole hours, or a half or quarter hour in a few zones. A skew within a couple of
/// minutes of such a boundary is far more likely to be a misconfigured `TZ` than a device whose clock
/// has drifted to exactly that value.
pub fn timezone_error_hint(skew: i64) -> Option<String> {
    const QUARTER_HOUR: i64 = 15 * 60;
    const TOLERANCE: i64 = 120;

    let magnitude = skew.saturating_abs();
    if magnitude < QUARTER_HOUR.saturating_sub(TOLERANCE) {
        return None;
    }

    // Round to the nearest quarter hour. Integer division is the intent: the quotient is which
    // quarter-hour boundary this skew is closest to.
    let nearest = magnitude
        .saturating_add(QUARTER_HOUR.checked_div(2)?)
        .checked_div(QUARTER_HOUR)?
        .saturating_mul(QUARTER_HOUR);
    if nearest == 0 || magnitude.saturating_sub(nearest).saturating_abs() > TOLERANCE {
        return None;
    }

    let hours = nearest.checked_div(3600)?;
    let minutes = nearest.checked_rem(3600)?.checked_div(60)?;
    let sign = if skew > 0 { "ahead of" } else { "behind" };
    Some(format!(
        "this is within two minutes of {hours:02}:{minutes:02}, so our clock is most likely \
         {sign} the device by a whole timezone offset — check TZ"
    ))
}

#[cfg(test)]
mod tests {
    use super::{SKEW_WARN_SECONDS, skew_seconds, system_local, timezone_error_hint};
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
        let now = system_local();
        assert!(now.is_plausible(), "{now}");
        assert!(now.year >= 2024, "year {} looks wrong", now.year);
    }

    #[test]
    fn skew_is_signed_and_in_seconds() {
        assert_eq!(skew_seconds(stamp(12, 0, 30), stamp(12, 0, 0)), Some(30));
        assert_eq!(skew_seconds(stamp(12, 0, 0), stamp(12, 0, 30)), Some(-30));
        assert_eq!(skew_seconds(stamp(12, 0, 0), stamp(12, 0, 0)), Some(0));
    }

    #[test]
    fn a_timezone_sized_error_is_diagnosed_as_one() {
        // A host on UTC against a device two hours ahead: the exact misconfiguration this guards.
        let skew = skew_seconds(stamp(12, 38, 41), stamp(10, 38, 41)).expect("same day");
        assert_eq!(skew, 7_200);
        assert!(skew.abs() > SKEW_WARN_SECONDS);

        let hint = timezone_error_hint(skew).expect("two hours is a zone offset");
        assert!(hint.contains("02:00"), "{hint}");
        assert!(hint.contains("TZ"), "{hint}");
        assert!(hint.contains("ahead of"), "{hint}");

        // The other direction reads the other way round.
        let hint = timezone_error_hint(-7_200).expect("still a zone offset");
        assert!(hint.contains("behind"), "{hint}");

        // Half-hour zones exist and should be recognised too.
        let hint = timezone_error_hint(5 * 3600 + 30 * 60).expect("India");
        assert!(hint.contains("05:30"), "{hint}");
    }

    #[test]
    fn ordinary_drift_is_not_blamed_on_the_timezone() {
        // Small skews, and skews nowhere near a quarter-hour boundary, are the device's clock drifting.
        for skew in [0, 5, -30, 61, -200, 700, 4_000, -11_000] {
            assert!(
                timezone_error_hint(skew).is_none(),
                "{skew}s should not be called a timezone error"
            );
        }
    }

    #[test]
    fn the_hint_keys_on_quarter_hours_including_a_few_that_are_not_real_zones() {
        // 9000 s is exactly 02:30. No such zone exists, but the heuristic keys on quarter-hour
        // multiples rather than a table of real offsets — a false positive on a diagnostic hint costs
        // an operator one glance at TZ, whereas maintaining a zone table costs forever.
        assert!(timezone_error_hint(9_000).is_some());
    }

    #[test]
    fn different_dates_are_not_worth_a_number() {
        let mut tomorrow = stamp(0, 0, 0);
        tomorrow.day = 9;
        assert_eq!(skew_seconds(stamp(23, 59, 59), tomorrow), None);
    }

    #[test]
    fn an_implausible_timestamp_yields_no_skew() {
        let mut bad = stamp(12, 0, 0);
        bad.month = 13;
        assert_eq!(skew_seconds(stamp(12, 0, 0), bad), None);
        assert_eq!(skew_seconds(bad, stamp(12, 0, 0)), None);
    }
}
