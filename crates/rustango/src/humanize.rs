//! Django `humanize` template filters as Tera filters. Issue #17.
//!
//! Seven filters that show up on every user-facing template:
//! `intcomma`, `intword`, `naturalsize`, `ordinal`, `apnumber`,
//! `naturaltime`, `naturalday`. Call [`register_filters`] on a Tera
//! instance to make them available:
//!
//! ```ignore
//! let mut tera = tera::Tera::default();
//! rustango::humanize::register_filters(&mut tera);
//! // now {{ count | intcomma }} renders "1,234,567"
//! ```
//!
//! Matches [Django humanize](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/)
//! output character-for-character on the English (en-US) locale.
//! Locale-aware formatting (German thousands-separator `.`, French
//! `intword` plural words, etc.) is deferred — gated on the
//! framework-wide timezone / locale issue. Until then, every filter
//! emits English output.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use tera::{to_value, Tera, Value};

/// Register every humanize filter on `tera`. Call from app setup
/// (typically right after `Tera::new(...)` / `Tera::default()`).
pub fn register_filters(tera: &mut Tera) {
    tera.register_filter("intcomma", intcomma);
    tera.register_filter("intword", intword);
    tera.register_filter("naturalsize", naturalsize);
    tera.register_filter("ordinal", ordinal);
    tera.register_filter("apnumber", apnumber);
    tera.register_filter("naturaltime", naturaltime);
    tera.register_filter("naturalday", naturalday);
    tera.register_filter("timesince", timesince);
    tera.register_filter("timeuntil", timeuntil);
}

// ------------------------------------------------------------------ intcomma

/// `intcomma` — insert thousands-separator commas. Django:
/// `4500 → "4,500"`, `1234567.89 → "1,234,567.89"`. Non-numeric input
/// passes through unchanged.
fn intcomma(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    if let Some(n) = value.as_i64() {
        return Ok(to_value(format_with_commas_i64(n))?);
    }
    if let Some(n) = value.as_u64() {
        return Ok(to_value(format_with_commas_u64(n))?);
    }
    if let Some(f) = value.as_f64() {
        // Keep the fractional part untouched; only comma-separate the
        // integer portion.
        let s = format!("{f}");
        if let Some((int_part, frac_part)) = s.split_once('.') {
            let int_with_commas = comma_separate_digits(int_part);
            return Ok(to_value(format!("{int_with_commas}.{frac_part}"))?);
        }
        return Ok(to_value(comma_separate_digits(&s))?);
    }
    Ok(value.clone())
}

fn format_with_commas_i64(n: i64) -> String {
    let s = n.abs().to_string();
    let sep = comma_separate_digits(&s);
    if n < 0 {
        format!("-{sep}")
    } else {
        sep
    }
}

fn format_with_commas_u64(n: u64) -> String {
    comma_separate_digits(&n.to_string())
}

/// Insert commas into a digit-string at every 3-digit boundary from the right.
fn comma_separate_digits(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ------------------------------------------------------------------ intword

/// `intword` — large numbers as words. Django:
/// `1_200_000 → "1.2 million"`, `1_000_000_000 → "1.0 billion"`.
/// Below 1 million the number passes through as-is.
fn intword(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v as f64,
        None => match value.as_f64() {
            Some(v) => v,
            None => return Ok(value.clone()),
        },
    };
    if n.abs() < 1_000_000.0 {
        // Django returns the int unformatted for <1M.
        if let Some(i) = value.as_i64() {
            return Ok(to_value(i.to_string())?);
        }
        return Ok(value.clone());
    }
    // Powers of 1000 above million. Django stops at quattuordecillion (1e48).
    // We cover the practical range; anything beyond falls back to e-notation.
    let scales: &[(f64, &str)] = &[
        (1e6, "million"),
        (1e9, "billion"),
        (1e12, "trillion"),
        (1e15, "quadrillion"),
        (1e18, "quintillion"),
        (1e21, "sextillion"),
        (1e24, "septillion"),
        (1e27, "octillion"),
        (1e30, "nonillion"),
        (1e33, "decillion"),
    ];
    // Pick the largest scale that fits.
    let mut chosen = scales[0];
    for &(s, name) in scales {
        if n.abs() >= s {
            chosen = (s, name);
        } else {
            break;
        }
    }
    let scaled = n / chosen.0;
    // Django format: one decimal place; trailing `.0` is preserved
    // (e.g. "1.0 billion", not "1 billion").
    Ok(to_value(format!("{:.1} {}", scaled, chosen.1))?)
}

// ------------------------------------------------------------------ naturalsize

/// `naturalsize` — bytes formatted human-readable (binary KiB-scale).
/// `1024 → "1.0 KB"`, `1536 → "1.5 KB"`, `1_572_864 → "1.5 MB"`.
/// Falls back to bytes for values < 1024.
fn naturalsize(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_u64() {
        Some(v) => v as f64,
        None => match value.as_i64() {
            Some(v) if v >= 0 => v as f64,
            _ => match value.as_f64() {
                Some(v) => v,
                None => return Ok(value.clone()),
            },
        },
    };
    let units = ["bytes", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if n < 1024.0 {
        // Singular byte: "1 byte"; plural "N bytes" for everything else.
        if (n - 1.0).abs() < f64::EPSILON {
            return Ok(to_value("1 byte")?);
        }
        return Ok(to_value(format!("{} bytes", n as u64))?);
    }
    let mut scale = 0_usize;
    let mut scaled = n;
    while scaled >= 1024.0 && scale < units.len() - 1 {
        scaled /= 1024.0;
        scale += 1;
    }
    Ok(to_value(format!("{:.1} {}", scaled, units[scale]))?)
}

// ------------------------------------------------------------------ ordinal

/// `ordinal` — append the appropriate English ordinal suffix.
/// `1 → "1st"`, `2 → "2nd"`, `3 → "3rd"`, `4 → "4th"`, `11 → "11th"`,
/// `21 → "21st"`. Negative numbers get the same suffix as their
/// absolute value.
fn ordinal(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    Ok(to_value(format!(
        "{n}{}",
        ordinal_suffix(n.unsigned_abs())
    ))?)
}

fn ordinal_suffix(n: u64) -> &'static str {
    // 11/12/13 are "th" — special-case the teens before falling
    // through to the last-digit branch.
    let last_two = n % 100;
    if (11..=13).contains(&last_two) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

// ------------------------------------------------------------------ apnumber

/// `apnumber` — spell out small numbers (1..9 → "one".."nine"),
/// pass others through unchanged. Matches the Associated Press style
/// guide that Django adopts.
fn apnumber(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    let word = match n {
        1 => "one",
        2 => "two",
        3 => "three",
        4 => "four",
        5 => "five",
        6 => "six",
        7 => "seven",
        8 => "eight",
        9 => "nine",
        _ => return Ok(to_value(n.to_string())?),
    };
    Ok(to_value(word)?)
}

// ------------------------------------------------------------------ naturaltime

/// `naturaltime` — relative time string compared to "now".
/// `"3 minutes ago"`, `"in 5 hours"`, `"just now"`. Accepts RFC3339
/// strings or anything serde-parsable as `DateTime<Utc>`.
///
/// Bucket thresholds match Django's `naturaltime`:
/// - <30s → "now"
/// - <60s → "N seconds {ago,from now}"
/// - <60m → "N minutes {ago,from now}"
/// - <24h → "N hours {ago,from now}"
/// - <30d → "N days {ago,from now}"
/// - <365d → "N months {ago,from now}"
/// - ≥365d → "N years {ago,from now}"
fn naturaltime(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    Ok(to_value(natural_time_string(now, dt))?)
}

fn parse_datetime(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = value.as_str() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    serde_json::from_value(value.clone()).ok()
}

fn natural_time_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let abs = delta.num_seconds().abs();
    let suffix = if delta.num_seconds() >= 0 {
        "ago"
    } else {
        "from now"
    };

    if abs < 30 {
        return "now".to_owned();
    }
    if abs < 60 {
        return format_unit(abs, "second", suffix);
    }
    let minutes = abs / 60;
    if minutes < 60 {
        return format_unit(minutes, "minute", suffix);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_unit(hours, "hour", suffix);
    }
    let days = hours / 24;
    if days < 30 {
        return format_unit(days, "day", suffix);
    }
    let months = days / 30;
    if months < 12 {
        return format_unit(months, "month", suffix);
    }
    let years = days / 365;
    format_unit(years, "year", suffix)
}

fn format_unit(n: i64, unit: &str, suffix: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    if suffix == "ago" {
        format!("{n} {unit}{plural} ago")
    } else {
        format!("in {n} {unit}{plural}")
    }
}

// ------------------------------------------------------------------ naturalday

/// `naturalday` — calendar-relative day name. `"today"`, `"yesterday"`,
/// `"tomorrow"`, else `"MMM DD"` (e.g. `"Apr 27"`). Matches Django's
/// default date format for the fallback case.
fn naturalday(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    Ok(to_value(natural_day_string(Utc::now(), dt))?)
}

fn natural_day_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let today = now.date_naive();
    let other = then.date_naive();
    let diff = (other - today).num_days();
    match diff {
        0 => "today".to_owned(),
        -1 => "yesterday".to_owned(),
        1 => "tomorrow".to_owned(),
        _ => {
            let month = match other.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => unreachable!(),
            };
            format!("{month} {:02}", other.day())
        }
    }
}

// ------------------------------------------------------------------ timesince / timeuntil

/// Magnitude-only equivalent of [`natural_time_string`]: emits
/// `"N units"` without an `"ago"` / `"in"` decorator. Used by
/// [`timesince`] / [`timeuntil`].
///
/// Returns `"0 minutes"` for non-positive deltas — Django's
/// `timesince` does the same (negative deltas indicate the page
/// rendered AFTER the target, which we treat as "no time has
/// passed yet").
fn magnitude_string(seconds: i64) -> String {
    if seconds <= 0 {
        return "0 minutes".to_owned();
    }
    if seconds < 60 {
        return format_magnitude(seconds, "second");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format_magnitude(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_magnitude(hours, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return format_magnitude(days, "day");
    }
    let months = days / 30;
    if months < 12 {
        return format_magnitude(months, "month");
    }
    let years = days / 365;
    format_magnitude(years, "year")
}

fn format_magnitude(n: i64, unit: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural}")
}

/// `timesince` — duration from `value` to now, formatted as
/// `"N units"`. Django's `{{ post.created | timesince }}` shape.
/// Returns `"0 minutes"` when the input is in the future (caller
/// likely wants [`timeuntil`] for that case).
///
/// Bucketing matches [`naturaltime`] — seconds / minutes / hours
/// / days / months (30-day) / years (365-day) — and pluralization
/// drops the trailing `s` only for `1`.
fn timesince(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = now.signed_duration_since(dt).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

/// `timeuntil` — duration from now to `value`, formatted as
/// `"N units"`. Mirror of [`timesince`] for future-pointing values:
/// `{{ event.start | timeuntil }}` → `"3 days"`. Past timestamps
/// produce `"0 minutes"`.
fn timeuntil(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = dt.signed_duration_since(now).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn render(tera: &Tera, src: &str, ctx: tera::Context) -> String {
        let mut t = tera.clone();
        t.add_raw_template("_", src).unwrap();
        t.render("_", &ctx).unwrap()
    }

    fn setup() -> Tera {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera
    }

    // -------- intcomma --------

    #[test]
    fn intcomma_handles_small_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &450_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "450");
    }

    #[test]
    fn intcomma_inserts_separators() {
        let tera = setup();
        for (n, expected) in [
            (1_234_i64, "1,234"),
            (1_234_567, "1,234,567"),
            (1_000_000_000, "1,000,000,000"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(
                render(&tera, "{{ n | intcomma }}", ctx),
                expected,
                "for n={n}"
            );
        }
    }

    #[test]
    fn intcomma_handles_negative_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &-1_234_567_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "-1,234,567");
    }

    #[test]
    fn intcomma_preserves_decimal_part() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_234_567.89_f64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,234,567.89");
    }

    // -------- intword --------

    #[test]
    fn intword_below_million_unchanged() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &999_999_i64);
        assert_eq!(render(&tera, "{{ n | intword }}", ctx), "999999");
    }

    #[test]
    fn intword_million_scale() {
        let tera = setup();
        for (n, expected) in [
            (1_200_000_i64, "1.2 million"),
            (1_000_000, "1.0 million"),
            (2_500_000_000, "2.5 billion"),
            (1_000_000_000_000_i64, "1.0 trillion"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | intword }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturalsize --------

    #[test]
    fn naturalsize_byte_threshold() {
        let tera = setup();
        for (bytes, expected) in [
            (0_u64, "0 bytes"),
            (1, "1 byte"),
            (512, "512 bytes"),
            (1_023, "1023 bytes"),
            (1_024, "1.0 KB"),
            (1_536, "1.5 KB"),
            (1_572_864, "1.5 MB"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("b", &bytes);
            assert_eq!(
                render(&tera, "{{ b | naturalsize }}", ctx),
                expected,
                "bytes={bytes}"
            );
        }
    }

    // -------- ordinal --------

    #[test]
    fn ordinal_picks_correct_suffix() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (10, "10th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (14, "14th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (101, "101st"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | ordinal }}", ctx), expected, "n={n}");
        }
    }

    // -------- apnumber --------

    #[test]
    fn apnumber_spells_one_through_nine() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "one"),
            (5, "five"),
            (9, "nine"),
            (10, "10"),
            (42, "42"),
            (0, "0"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | apnumber }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturaltime --------

    #[test]
    fn naturaltime_buckets_correctly() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        for (offset_secs, expected) in [
            (5, "now"),
            (45, "45 seconds ago"),
            (-45, "in 45 seconds"),
            (60, "1 minute ago"),
            (120, "2 minutes ago"),
            (3600, "1 hour ago"),
            (7200, "2 hours ago"),
            (86_400, "1 day ago"),
            (86_400 * 2, "2 days ago"),
            (86_400 * 31, "1 month ago"),
            (86_400 * 400, "1 year ago"),
            (-3600, "in 1 hour"),
        ] {
            let then = now - Duration::seconds(offset_secs);
            assert_eq!(
                natural_time_string(now, then),
                expected,
                "offset={offset_secs}"
            );
        }
    }

    // -------- naturalday --------

    #[test]
    fn naturalday_keywords() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        let today = now;
        let yesterday = now - Duration::days(1);
        let tomorrow = now + Duration::days(1);
        let week_ago = now - Duration::days(7);
        assert_eq!(natural_day_string(now, today), "today");
        assert_eq!(natural_day_string(now, yesterday), "yesterday");
        assert_eq!(natural_day_string(now, tomorrow), "tomorrow");
        // 7 days back from 2026-05-16 = 2026-05-09
        assert_eq!(natural_day_string(now, week_ago), "May 09");
    }

    // -------- register --------

    #[test]
    fn register_filters_makes_them_callable_via_tera() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_000_000_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,000,000");
    }

    // -------- timesince / timeuntil --------

    #[test]
    fn magnitude_string_buckets_match_naturaltime() {
        assert_eq!(magnitude_string(0), "0 minutes");
        assert_eq!(magnitude_string(-5), "0 minutes");
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(45), "45 seconds");
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
        assert_eq!(magnitude_string(60 * 60), "1 hour");
        assert_eq!(magnitude_string(60 * 60 * 5), "5 hours");
        assert_eq!(magnitude_string(60 * 60 * 24), "1 day");
        assert_eq!(magnitude_string(60 * 60 * 24 * 31), "1 month");
        assert_eq!(magnitude_string(60 * 60 * 24 * 366), "1 year");
    }

    #[test]
    fn timesince_filter_emits_magnitude_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // 2 hours ago, give-or-take.
        let then = Utc::now() - Duration::hours(2);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timesince }}", ctx);
        assert_eq!(out, "2 hours");
    }

    #[test]
    fn timesince_filter_emits_zero_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let later = Utc::now() + Duration::hours(2);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timesince }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timeuntil_filter_emits_magnitude_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // Use a wider gap so test-wall-clock-drift between insert
        // and render-time doesn't bump us across the day boundary.
        let later = Utc::now() + Duration::days(3) + Duration::hours(1);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timeuntil }}", ctx);
        assert_eq!(out, "3 days", "got: {out}");
    }

    #[test]
    fn timeuntil_filter_emits_zero_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let then = Utc::now() - Duration::days(3);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timeuntil }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timesince_pluralizes_correctly() {
        // 1 second → "1 second"; 2 → "2 seconds"
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(2), "2 seconds");
        // 1 minute → "1 minute"; 2 → "2 minutes"
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
    }
}
