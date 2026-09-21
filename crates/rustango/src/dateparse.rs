//! ISO 8601 / W3C date and time parsers.
//!
//! Each function returns `Option<T>` and gives `None` when the input
//! does not parse. Use them on strings from JSON bodies, form fields
//! and query strings.
//!
//! ```ignore
//! use rustango::dateparse::{parse_date, parse_time, parse_datetime, parse_duration};
//!
//! assert!(parse_date("2026-06-04").is_some());
//! assert!(parse_time("12:30:00").is_some());
//! assert!(parse_datetime("2026-06-04T12:30:00Z").is_some());
//! assert!(parse_duration("PT1H30M").is_some()); // ISO 8601 duration
//! assert!(parse_duration("1 day, 2:30:00").is_some()); // clock shape
//! ```

use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

/// Parse an ISO 8601 date (`YYYY-MM-DD`) into a `NaiveDate`.
///
/// Out-of-range dates such as `2026-02-30` or `2026-13-01` give
/// `None`. Surrounding whitespace is not allowed, so trim it first.
///
/// ```ignore
/// use rustango::dateparse::parse_date;
/// assert_eq!(
///     parse_date("2026-06-04").map(|d| d.to_string()),
///     Some("2026-06-04".to_owned())
/// );
/// assert!(parse_date("not a date").is_none());
/// assert!(parse_date("2026-02-30").is_none());
/// ```
#[must_use]
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Parse an ISO 8601 time (`HH:MM:SS[.fff]`) into a `NaiveTime`.
/// Fractional seconds are optional. A timezone is not accepted.
///
/// ```ignore
/// use rustango::dateparse::parse_time;
/// assert!(parse_time("12:30:00").is_some());
/// assert!(parse_time("12:30:00.123").is_some());
/// assert!(parse_time("25:00:00").is_none()); // hour out of range
/// ```
#[must_use]
pub fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

/// Parse an ISO 8601 datetime
/// (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`) into a `DateTime<Utc>`.
/// The date and time may be split by `T` or by a space. RFC 2822 is
/// also accepted.
///
/// An input with no zone is read as UTC.
///
/// ```ignore
/// use rustango::dateparse::parse_datetime;
/// assert!(parse_datetime("2026-06-04T12:30:00Z").is_some());
/// assert!(parse_datetime("2026-06-04T12:30:00+02:00").is_some());
/// assert!(parse_datetime("2026-06-04 12:30:00").is_some()); // space separator
/// assert!(parse_datetime("2026-06-04T12:30:00").is_some());  // naive → UTC
/// assert!(parse_datetime("garbage").is_none());
/// ```
#[must_use]
pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    // 1. RFC 3339 / ISO 8601 with an explicit zone (Z or ±HH:MM).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // 2. RFC 2822, which some APIs emit.
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // 3. No zone: `T` or a space between date and time.
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Render a `Duration` as a clock-style duration string: the
/// inverse of [`parse_duration`].
///
/// The output is `"<n> day[s], HH:MM:SS"` when there is at least one
/// day, else `"HH:MM:SS"`. A sub-second part is added as `.ffffff`.
///
/// ```ignore
/// use std::time::Duration;
/// use rustango::dateparse::duration_string;
/// assert_eq!(duration_string(Duration::from_secs(9000)), "02:30:00");
/// assert_eq!(duration_string(Duration::from_secs(86400 + 9000)), "1 day, 02:30:00");
/// assert_eq!(duration_string(Duration::from_secs(7 * 86400)), "7 days, 00:00:00");
/// assert_eq!(duration_string(Duration::ZERO), "00:00:00");
/// ```
#[must_use]
pub fn duration_string(d: Duration) -> String {
    let total = d.as_secs();
    let nanos = d.subsec_nanos();
    let days = total / 86_400;
    let rem = total % 86_400;
    let hours = rem / 3_600;
    let mins = (rem % 3_600) / 60;
    let secs = rem % 60;

    let mut out = String::with_capacity(32);
    match days {
        0 => {}
        1 => out.push_str("1 day, "),
        n => {
            use std::fmt::Write as _;
            let _ = write!(out, "{n} days, ");
        }
    }
    use std::fmt::Write as _;
    let _ = write!(out, "{hours:02}:{mins:02}:{secs:02}");
    if nanos > 0 {
        // The fractional part is 6 digits, so use microseconds.
        let micros = nanos / 1000;
        let _ = write!(out, ".{micros:06}");
    }
    out
}

/// ISO 8601 duration string for a [`std::time::Duration`].
///
/// The shape is `P<days>DT<HH>H<MM>M<SS>[.<micros>]S`. The days and
/// all three time fields are always printed; the `.ffffff` tail only
/// appears for a sub-second value, so
/// `Duration::ZERO` gives `"P0DT00H00M00S"`, not `"PT0S"`.
///
/// For the `"N days, HH:MM:SS"` form use [`duration_string`].
///
/// ```
/// use std::time::Duration;
/// use rustango::dateparse::duration_iso_string;
///
/// assert_eq!(duration_iso_string(Duration::ZERO), "P0DT00H00M00S");
/// assert_eq!(duration_iso_string(Duration::from_secs(9000)), "P0DT02H30M00S");
/// assert_eq!(
///     duration_iso_string(Duration::from_secs(86400 + 9000)),
///     "P1DT02H30M00S"
/// );
/// assert_eq!(
///     duration_iso_string(Duration::from_micros(123_456)),
///     "P0DT00H00M00.123456S"
/// );
/// ```
#[must_use]
pub fn duration_iso_string(d: Duration) -> String {
    let total = d.as_secs();
    let nanos = d.subsec_nanos();
    let days = total / 86_400;
    let rem = total % 86_400;
    let hours = rem / 3_600;
    let mins = (rem % 3_600) / 60;
    let secs = rem % 60;
    use std::fmt::Write as _;
    let mut out = String::with_capacity(24);
    let _ = write!(out, "P{days}DT{hours:02}H{mins:02}M{secs:02}");
    if nanos > 0 {
        let micros = nanos / 1000;
        let _ = write!(out, ".{micros:06}");
    }
    out.push('S');
    out
}

/// Parse a duration string into a `std::time::Duration`. Two shapes
/// are accepted:
///
/// 1. **Clock shape**: `"[D days, ]H:MM:SS[.fff]"`, for example
///    `"1 day, 02:30:00"` or `"02:30:00.500"`.
/// 2. **ISO 8601**: `"PT1H30M"`, `"P1DT12H"`, `"PT45.5S"`.
///
/// Anything else gives `None`. A negative duration is never
/// accepted, because `std::time::Duration` is unsigned.
///
/// ```ignore
/// use rustango::dateparse::parse_duration;
/// use std::time::Duration;
///
/// assert_eq!(parse_duration("02:30:00"), Some(Duration::from_secs(9000)));
/// assert_eq!(parse_duration("1 day, 02:30:00"), Some(Duration::from_secs(86400 + 9000)));
/// assert_eq!(parse_duration("PT1H30M"), Some(Duration::from_secs(5400)));
/// assert_eq!(parse_duration("garbage"), None);
/// ```
#[must_use]
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // ISO 8601: `P[nD]T[nH][nM][n.nS]`, or `P[nD]` with no T part.
    if s.starts_with('P') || s.starts_with('p') {
        return parse_iso_duration(s);
    }
    parse_clock_duration(s)
}

fn parse_clock_duration(s: &str) -> Option<Duration> {
    // Optional leading `N day,` / `N days,` segment.
    let (days, rest) = match s.find(", ") {
        Some(idx) => {
            let prefix = &s[..idx];
            let suffix = &s[idx + 2..];
            let stripped = prefix
                .trim_end_matches("days")
                .trim_end_matches("day")
                .trim();
            let d: u64 = stripped.parse().ok()?;
            (d, suffix)
        }
        None => (0u64, s),
    };
    // `HH:MM:SS[.frac]` body.
    let (hms, frac_secs) = match rest.find('.') {
        Some(dot) => (&rest[..dot], parse_frac(&rest[dot..])?),
        None => (rest, 0u32),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let sec: u64 = parts[2].parse().ok()?;
    if m >= 60 || sec >= 60 {
        return None;
    }
    let total_secs = days
        .checked_mul(86_400)?
        .checked_add(h.checked_mul(3_600)?)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(sec)?;
    Some(Duration::new(total_secs, frac_secs))
}

/// Parse a `.fff` fractional-second part into nanoseconds.
fn parse_frac(dot_and_digits: &str) -> Option<u32> {
    let digits = dot_and_digits.strip_prefix('.')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Pad/truncate to 9 digits (nanoseconds).
    let mut padded = String::with_capacity(9);
    padded.push_str(digits);
    while padded.len() < 9 {
        padded.push('0');
    }
    padded[..9].parse().ok()
}

fn parse_iso_duration(s: &str) -> Option<Duration> {
    // Strip leading P (or p).
    let body = &s[1..];
    let has_t = body.contains('T');
    let (days_part, time_part) = match body.find('T') {
        Some(idx) => (&body[..idx], &body[idx + 1..]),
        None => (body, ""),
    };
    // Reject `P` and `PT`: one of the two parts must carry a value.
    if days_part.is_empty() && (!has_t || time_part.is_empty()) {
        return None;
    }
    if has_t && time_part.is_empty() {
        return None;
    }
    let mut total_secs: u64 = 0;
    let mut nanos: u32 = 0;

    // Only `nD` is accepted here. Weeks, months and years depend on
    // the calendar, so a plain `Duration` cannot hold them.
    if !days_part.is_empty() {
        let d = days_part
            .strip_suffix('D')
            .or_else(|| days_part.strip_suffix('d'))?;
        let n: u64 = d.parse().ok()?;
        total_secs = total_secs.checked_add(n.checked_mul(86_400)?)?;
    }

    if !time_part.is_empty() {
        let mut cursor = time_part;
        while !cursor.is_empty() {
            let split_pos = cursor
                .char_indices()
                .find(|&(_, c)| matches!(c, 'H' | 'h' | 'M' | 'm' | 'S' | 's'))?;
            let (num_str, rest) = cursor.split_at(split_pos.0);
            let unit_char = split_pos.1;
            cursor = &rest[unit_char.len_utf8()..];
            match unit_char {
                'H' | 'h' => {
                    let n: u64 = num_str.parse().ok()?;
                    total_secs = total_secs.checked_add(n.checked_mul(3_600)?)?;
                }
                'M' | 'm' => {
                    let n: u64 = num_str.parse().ok()?;
                    total_secs = total_secs.checked_add(n.checked_mul(60)?)?;
                }
                'S' | 's' => {
                    // Seconds may be fractional.
                    if let Some(dot) = num_str.find('.') {
                        let whole: u64 = num_str[..dot].parse().ok()?;
                        nanos = parse_frac(&num_str[dot..])?;
                        total_secs = total_secs.checked_add(whole)?;
                    } else {
                        let n: u64 = num_str.parse().ok()?;
                        total_secs = total_secs.checked_add(n)?;
                    }
                }
                _ => return None,
            }
        }
    }
    Some(Duration::new(total_secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- parse_date --------

    #[test]
    fn date_iso_8601() {
        let d = parse_date("2026-06-04").unwrap();
        assert_eq!(d.to_string(), "2026-06-04");
    }

    #[test]
    fn date_rejects_garbage() {
        assert!(parse_date("not a date").is_none());
        assert!(parse_date("").is_none());
    }

    #[test]
    fn date_rejects_out_of_range() {
        assert!(parse_date("2026-02-30").is_none()); // Feb has 28/29
        assert!(parse_date("2026-13-01").is_none()); // month overflow
        assert!(parse_date("2026-00-15").is_none()); // month zero
    }

    #[test]
    fn date_rejects_non_iso_shapes() {
        // US-format mm/dd/yyyy — rejected.
        assert!(parse_date("06/04/2026").is_none());
        // Dotted — rejected.
        assert!(parse_date("2026.06.04").is_none());
    }

    // -------- parse_time --------

    #[test]
    fn time_basic() {
        let t = parse_time("12:30:00").unwrap();
        assert_eq!(t.hour(), 12);
        assert_eq!(t.minute(), 30);
        assert_eq!(t.second(), 0);
    }

    #[test]
    fn time_with_fractional_seconds() {
        let t = parse_time("12:30:00.123").unwrap();
        // 123 ms = 123_000_000 ns
        assert_eq!(t.nanosecond(), 123_000_000);
    }

    #[test]
    fn time_rejects_out_of_range() {
        assert!(parse_time("25:00:00").is_none());
        assert!(parse_time("12:60:00").is_none());
    }

    // -------- parse_datetime --------

    #[test]
    fn datetime_rfc3339_with_z() {
        let dt = parse_datetime("2026-06-04T12:30:00Z").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-06-04 12:30:00"
        );
    }

    #[test]
    fn datetime_rfc3339_with_offset() {
        // +02:00 should be normalized to UTC (subtract 2 hours).
        let dt = parse_datetime("2026-06-04T12:30:00+02:00").unwrap();
        assert_eq!(dt.format("%H:%M").to_string(), "10:30");
    }

    #[test]
    fn datetime_space_separator() {
        let dt = parse_datetime("2026-06-04 12:30:00").unwrap();
        assert_eq!(dt.format("%H:%M:%S").to_string(), "12:30:00");
    }

    #[test]
    fn datetime_naive_treated_as_utc() {
        let dt = parse_datetime("2026-06-04T12:30:00").unwrap();
        assert_eq!(dt.format("%H:%M").to_string(), "12:30");
    }

    #[test]
    fn datetime_rejects_garbage() {
        assert!(parse_datetime("not a datetime").is_none());
        assert!(parse_datetime("").is_none());
    }

    // -------- parse_duration: clock shape --------

    #[test]
    fn duration_hms_only() {
        let d = parse_duration("02:30:00").unwrap();
        assert_eq!(d.as_secs(), 9000);
    }

    #[test]
    fn duration_with_days() {
        let d = parse_duration("1 day, 02:30:00").unwrap();
        assert_eq!(d.as_secs(), 86_400 + 9000);
    }

    #[test]
    fn duration_with_multiple_days() {
        let d = parse_duration("7 days, 00:00:00").unwrap();
        assert_eq!(d.as_secs(), 7 * 86_400);
    }

    #[test]
    fn duration_with_fractional_seconds() {
        let d = parse_duration("00:00:01.500").unwrap();
        assert_eq!(d.as_secs(), 1);
        assert_eq!(d.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn duration_rejects_60_minute() {
        assert!(parse_duration("00:60:00").is_none());
        assert!(parse_duration("00:00:60").is_none());
    }

    // -------- parse_duration: ISO 8601 --------

    #[test]
    fn duration_iso_hours_minutes() {
        let d = parse_duration("PT1H30M").unwrap();
        assert_eq!(d.as_secs(), 5400);
    }

    #[test]
    fn duration_iso_with_days() {
        let d = parse_duration("P1DT12H").unwrap();
        assert_eq!(d.as_secs(), 86_400 + 12 * 3600);
    }

    #[test]
    fn duration_iso_seconds_only() {
        let d = parse_duration("PT45S").unwrap();
        assert_eq!(d.as_secs(), 45);
    }

    #[test]
    fn duration_iso_fractional_seconds() {
        let d = parse_duration("PT45.500S").unwrap();
        assert_eq!(d.as_secs(), 45);
        assert_eq!(d.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn duration_iso_days_only() {
        let d = parse_duration("P3D").unwrap();
        assert_eq!(d.as_secs(), 3 * 86_400);
    }

    #[test]
    fn duration_empty_returns_none() {
        assert!(parse_duration("").is_none());
        assert!(parse_duration("   ").is_none());
    }

    #[test]
    fn duration_garbage_returns_none() {
        assert!(parse_duration("garbage").is_none());
        assert!(parse_duration("PT").is_none());
        assert!(parse_duration("PXY").is_none());
    }

    use chrono::Timelike as _;

    // -------- duration_string (inverse of parse_duration) --------

    #[test]
    fn duration_string_zero() {
        assert_eq!(duration_string(Duration::ZERO), "00:00:00");
    }

    #[test]
    fn duration_string_hms_only() {
        assert_eq!(duration_string(Duration::from_secs(9000)), "02:30:00");
    }

    #[test]
    fn duration_string_one_day_singular() {
        // "1 day, " (singular) vs "N days, " (plural).
        assert_eq!(
            duration_string(Duration::from_secs(86400 + 9000)),
            "1 day, 02:30:00"
        );
    }

    #[test]
    fn duration_string_multiple_days_plural() {
        assert_eq!(
            duration_string(Duration::from_secs(7 * 86400)),
            "7 days, 00:00:00"
        );
    }

    #[test]
    fn duration_string_fractional_seconds() {
        // 1.5 seconds → "00:00:01.500000" (6-digit microsecond precision).
        let d = Duration::new(1, 500_000_000);
        assert_eq!(duration_string(d), "00:00:01.500000");
    }

    #[test]
    fn duration_string_round_trips_through_parse_duration() {
        // Symmetric: format → parse → format produces same string.
        for secs in [0u64, 60, 3600, 9000, 86_400, 86_400 + 9000, 7 * 86_400] {
            let s = duration_string(Duration::from_secs(secs));
            let parsed = parse_duration(&s).unwrap();
            assert_eq!(parsed.as_secs(), secs, "round-trip failed at {secs} secs");
        }
    }

    // -------- duration_iso_string --------

    #[test]
    fn duration_iso_zero() {
        assert_eq!(duration_iso_string(Duration::ZERO), "P0DT00H00M00S");
    }

    #[test]
    fn duration_iso_string_seconds_only() {
        assert_eq!(
            duration_iso_string(Duration::from_secs(45)),
            "P0DT00H00M45S"
        );
    }

    #[test]
    fn duration_iso_string_minutes_and_hours() {
        assert_eq!(
            duration_iso_string(Duration::from_secs(9000)),
            "P0DT02H30M00S"
        );
        assert_eq!(
            duration_iso_string(Duration::from_secs(3600)),
            "P0DT01H00M00S"
        );
    }

    #[test]
    fn duration_iso_string_with_days() {
        assert_eq!(
            duration_iso_string(Duration::from_secs(86_400 + 9000)),
            "P1DT02H30M00S"
        );
        assert_eq!(
            duration_iso_string(Duration::from_secs(7 * 86_400)),
            "P7DT00H00M00S"
        );
    }

    #[test]
    fn duration_iso_microseconds_only_when_subsecond() {
        // Whole-second value → no .ffffff tail.
        assert_eq!(duration_iso_string(Duration::from_secs(5)), "P0DT00H00M05S");
        // 123456 microseconds → 6-digit tail.
        assert_eq!(
            duration_iso_string(Duration::from_micros(123_456)),
            "P0DT00H00M00.123456S"
        );
        // 1 microsecond → leading zeros preserved.
        assert_eq!(
            duration_iso_string(Duration::from_micros(1)),
            "P0DT00H00M00.000001S"
        );
    }

    #[test]
    fn duration_iso_round_trips_through_parse_duration() {
        // Symmetric: ISO format → parse → format produces same string.
        for secs in [0u64, 45, 60, 3600, 9000, 86_400, 86_400 + 9000, 7 * 86_400] {
            let s = duration_iso_string(Duration::from_secs(secs));
            let parsed = parse_duration(&s).unwrap();
            assert_eq!(parsed.as_secs(), secs, "ISO round-trip failed at {secs} s");
        }
    }
}
