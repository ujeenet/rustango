//! Human-readable durations, like `django.utils.timesince`.
//!
//! [`timesince`] gives the time from a past moment up to now, as a
//! string such as `"4 days, 6 hours"`. [`timeuntil`] does the same
//! for a future moment.
//!
//! * Units are year, month, week, day, hour and minute. A year is
//!   365 days and a month is 30 days. Seconds are never shown.
//! * `depth` is how many units to print. `2` gives
//!   `"4 days, 6 hours"`, `1` gives `"4 days"`.
//! * A delta that is zero, negative or under a minute gives
//!   `"0 minutes"`.
//!
//! [`crate::humanize`] has Tera filters with a simpler one-unit
//! format; this module is the Rust API.
//!
//! # Example
//!
//! ```
//! use chrono::{TimeZone, Utc};
//! use rustango::timesince::{timesince, timeuntil};
//!
//! let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
//! let past = now - chrono::Duration::seconds(4 * 86400 + 6 * 3600);
//! assert_eq!(timesince(past, Some(now), 2), "4 days, 6 hours");
//!
//! let future = now + chrono::Duration::seconds(2 * 3600 + 30 * 60);
//! assert_eq!(timeuntil(future, Some(now), 2), "2 hours, 30 minutes");
//! ```
//!
//! [`timeuntil`]: crate::timesince::timeuntil

use chrono::{DateTime, Utc};

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
// Flat 30- and 365-day buckets, the same as Django's CHUNKS table.
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

const CHUNKS: &[(i64, &str)] = &[
    (YEAR, "year"),
    (MONTH, "month"),
    (WEEK, "week"),
    (DAY, "day"),
    (HOUR, "hour"),
    (MINUTE, "minute"),
];

fn format_unit(n: i64, unit: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural}")
}

fn build(seconds: i64, depth: usize) -> String {
    if seconds <= 0 {
        return "0 minutes".to_owned();
    }
    let depth = depth.clamp(1, CHUNKS.len());
    let mut remaining = seconds;
    let mut parts: Vec<String> = Vec::with_capacity(depth);
    let mut started = false;
    for (size, name) in CHUNKS {
        if !started {
            if remaining >= *size {
                let n = remaining / size;
                remaining -= n * size;
                parts.push(format_unit(n, name));
                started = true;
            }
            continue;
        }
        if parts.len() >= depth {
            break;
        }
        if remaining >= *size {
            let n = remaining / size;
            remaining -= n * size;
            parts.push(format_unit(n, name));
        }
    }
    if parts.is_empty() {
        // Less than a minute.
        return "0 minutes".to_owned();
    }
    parts.join(", ")
}

/// How long ago `d` was, counted up to `now`.
///
/// `now` defaults to `Utc::now()`. `depth` is how many units to
/// print, clamped to `1..=6`. Returns `"0 minutes"` if `d` is in the
/// future or less than a minute ago.
pub fn timesince(d: DateTime<Utc>, now: Option<DateTime<Utc>>, depth: usize) -> String {
    let now = now.unwrap_or_else(Utc::now);
    let delta = now.signed_duration_since(d).num_seconds();
    build(delta, depth)
}

/// How far ahead `d` is, counted from `now`. The mirror of
/// [`timesince`].
///
/// `now` defaults to `Utc::now()`. Returns `"0 minutes"` if `d` is
/// in the past or less than a minute away.
pub fn timeuntil(d: DateTime<Utc>, now: Option<DateTime<Utc>>, depth: usize) -> String {
    let now = now.unwrap_or_else(Utc::now);
    let delta = d.signed_duration_since(now).num_seconds();
    build(delta, depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn timesince_basic_depth_two() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(4 * DAY + 6 * HOUR);
        assert_eq!(timesince(past, Some(now), 2), "4 days, 6 hours");
    }

    #[test]
    fn timesince_depth_one_collapses() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(4 * DAY + 6 * HOUR);
        assert_eq!(timesince(past, Some(now), 1), "4 days");
    }

    #[test]
    fn timesince_singular_pluralization() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(DAY + HOUR);
        assert_eq!(timesince(past, Some(now), 2), "1 day, 1 hour");
    }

    #[test]
    fn timesince_skips_zero_intermediate_unit() {
        // Empty buckets between year and hour are skipped.
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(YEAR + 5 * HOUR);
        assert_eq!(timesince(past, Some(now), 2), "1 year, 5 hours");
    }

    #[test]
    fn timesince_future_returns_zero() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let future = now + Duration::seconds(3 * DAY);
        assert_eq!(timesince(future, Some(now), 2), "0 minutes");
    }

    #[test]
    fn timesince_sub_minute_returns_zero() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(30);
        assert_eq!(timesince(past, Some(now), 2), "0 minutes");
    }

    #[test]
    fn timeuntil_basic() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let future = now + Duration::seconds(2 * HOUR + 30 * MINUTE);
        assert_eq!(timeuntil(future, Some(now), 2), "2 hours, 30 minutes");
    }

    #[test]
    fn timeuntil_past_returns_zero() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(2 * HOUR);
        assert_eq!(timeuntil(past, Some(now), 2), "0 minutes");
    }

    #[test]
    fn timesince_depth_clamps_high() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now
            - Duration::seconds(2 * YEAR + 3 * MONTH + 4 * WEEK + 5 * DAY + 6 * HOUR + 7 * MINUTE);
        // depth=99 clamps to 6, the number of units.
        let s = timesince(past, Some(now), 99);
        assert!(s.contains("year"));
        assert!(s.contains("hour") || s.contains("minute"));
    }

    #[test]
    fn timesince_depth_clamps_zero() {
        // depth=0 clamps up to 1.
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(2 * DAY + 5 * HOUR);
        assert_eq!(timesince(past, Some(now), 0), "2 days");
    }

    #[test]
    fn timesince_weeks_bucket() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(2 * WEEK + 3 * DAY);
        assert_eq!(timesince(past, Some(now), 2), "2 weeks, 3 days");
    }

    #[test]
    fn timesince_months_bucket() {
        let now = t(2026, 6, 5, 12, 0, 0);
        let past = now - Duration::seconds(2 * MONTH + 5 * DAY);
        assert_eq!(timesince(past, Some(now), 2), "2 months, 5 days");
    }

    #[test]
    fn timesince_now_none_uses_utc_now() {
        // `None` falls back to Utc::now().
        let past = Utc::now() - Duration::seconds(2 * DAY + 5 * HOUR);
        let s = timesince(past, None, 2);
        assert!(s.contains("day"));
    }
}
