//! Django-shape date format-character expander.
//!
//! Mirrors `django.utils.dateformat` — converts a Django template
//! `{{ obj|date:"Y-m-d H:i" }}` format string into the rendered
//! output for a given `DateTime<Utc>` (or `NaiveDate` / `NaiveTime`).
//!
//! Django uses a single-character format-code language distinct
//! from strftime. The most common codes:
//!
//! | Code | Meaning                       | Example       |
//! |------|-------------------------------|---------------|
//! | `Y`  | 4-digit year                  | `2026`        |
//! | `y`  | 2-digit year                  | `26`          |
//! | `m`  | 2-digit month                 | `06`          |
//! | `n`  | 1-2-digit month               | `6`           |
//! | `M`  | Short month name              | `Jun`         |
//! | `F`  | Full month name               | `June`        |
//! | `d`  | 2-digit day                   | `04`          |
//! | `j`  | 1-2-digit day                 | `4`           |
//! | `D`  | Short weekday name            | `Thu`         |
//! | `l`  | Full weekday name             | `Thursday`    |
//! | `N`  | Day of year (1-366)           | `155`         |
//! | `w`  | Day of week, Sunday=0         | `4`           |
//! | `H`  | 2-digit 24-hour               | `13`          |
//! | `G`  | 1-2-digit 24-hour             | `13`          |
//! | `h`  | 2-digit 12-hour               | `01`          |
//! | `g`  | 1-2-digit 12-hour             | `1`           |
//! | `i`  | 2-digit minute                | `05`          |
//! | `s`  | 2-digit second                | `09`          |
//! | `a`  | `am` / `pm`                   | `pm`          |
//! | `A`  | `AM` / `PM`                   | `PM`          |
//! | `U`  | Unix epoch seconds            | `1717504800`  |
//! | `\X` | Literal character (escape)    | `X`           |
//!
//! Other Django codes (`b`, `e`, `I`, `O`, `T`, `Z`, `c`, `r`,
//! `u`, `o`, `t`, `S`, `f`, `P`, `W`) are recognized — see the
//! format-char dispatch in [`format_datetime`] for the exhaustive
//! list. `P` emits Django's "p.m." / "noon" / "midnight" form;
//! `f` collapses zero-minute times to "1" instead of "1:00"; `W`
//! is the ISO-8601 week number; `t` is days-in-month; `o` is the
//! ISO-8601 week-numbering year (differs from `Y` near year
//! boundaries).
//!
//! ```ignore
//! use chrono::{DateTime, Utc, TimeZone};
//! use rustango::dateformat::format_datetime;
//!
//! let dt: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 4, 13, 5, 9).unwrap();
//! assert_eq!(format_datetime(&dt, "Y-m-d H:i:s"), "2026-06-04 13:05:09");
//! assert_eq!(format_datetime(&dt, r"\Y\e\a\r: Y"), "Year: 2026");
//! ```

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike, Utc};

/// Format a `DateTime<Utc>` per Django's `dateformat` shape.
/// `format_string` uses Django's single-char codes (see module-level
/// doc table). Backslash escapes the next character — useful for
/// rendering literal letters that would otherwise be interpreted
/// (e.g. `r"\Y\e\a\r: Y"` → `"Year: 2026"`).
///
/// Unknown / unsupported chars in the format string pass through
/// unchanged (Django shape — `"random"` renders as `"random"`).
#[must_use]
pub fn format_datetime(dt: &DateTime<Utc>, format_string: &str) -> String {
    let mut out = String::with_capacity(format_string.len() * 2);
    let mut chars = format_string.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Literal escape — next char passes through verbatim.
            if let Some(literal) = chars.next() {
                out.push(literal);
            }
            continue;
        }
        match c {
            'Y' => out.push_str(&format!("{:04}", dt.year())),
            'y' => out.push_str(&format!("{:02}", dt.year() % 100)),
            'm' => out.push_str(&format!("{:02}", dt.month())),
            'n' => out.push_str(&format!("{}", dt.month())),
            'M' => out.push_str(short_month(dt.month())),
            'F' => out.push_str(full_month(dt.month())),
            'b' => out.push_str(&short_month(dt.month()).to_lowercase()),
            'd' => out.push_str(&format!("{:02}", dt.day())),
            'j' => out.push_str(&format!("{}", dt.day())),
            'D' => out.push_str(short_weekday(dt.weekday())),
            'l' => out.push_str(full_weekday(dt.weekday())),
            'N' => out.push_str(crate::dates::month_ap(dt.month())),
            'z' => out.push_str(&format!("{}", dt.ordinal())),
            'w' => out.push_str(&format!("{}", dt.weekday().num_days_from_sunday())),
            'H' => out.push_str(&format!("{:02}", dt.hour())),
            'G' => out.push_str(&format!("{}", dt.hour())),
            'h' => {
                let h = ((dt.hour() + 11) % 12) + 1;
                out.push_str(&format!("{h:02}"));
            }
            'g' => {
                let h = ((dt.hour() + 11) % 12) + 1;
                out.push_str(&format!("{h}"));
            }
            'i' => out.push_str(&format!("{:02}", dt.minute())),
            's' => out.push_str(&format!("{:02}", dt.second())),
            'a' => out.push_str(if dt.hour() < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if dt.hour() < 12 { "AM" } else { "PM" }),
            'U' => out.push_str(&format!("{}", dt.timestamp())),
            'c' => out.push_str(&dt.to_rfc3339()),
            'r' => out.push_str(&dt.to_rfc2822()),
            'S' => out.push_str(day_suffix(dt.day())),
            'L' => out.push(if is_leap(dt.year()) { '1' } else { '0' }),
            // Timezone codes — DateTime<Utc> always renders these
            // as UTC equivalents.
            'O' => out.push_str("+0000"),
            'T' => out.push_str("UTC"),
            'Z' => out.push_str("0"),
            'e' => out.push_str("UTC"),
            'I' => out.push('0'), // DST flag — always 0 for UTC
            // Microseconds — 6 digits of fractional seconds.
            'u' => out.push_str(&format!("{:06}", dt.nanosecond() / 1000)),
            // Django's `P` — "p.m." / "a.m." form with "noon" and
            // "midnight" specials; minutes shown only when non-zero.
            'P' => out.push_str(&fmt_pretty_meridiem(dt.hour(), dt.minute())),
            // Django's `f` — 12-hour hour, plus `:MM` only when MM
            // is non-zero. "1" / "1:30" / "12" (midnight or noon).
            'f' => out.push_str(&fmt_12h_minutes(dt.hour(), dt.minute())),
            // Django's `W` — ISO-8601 week number (1..=53).
            'W' => out.push_str(&format!("{}", dt.iso_week().week())),
            // Django's `t` — days in the current month (28..=31).
            't' => out.push_str(&format!("{}", days_in_month(dt.year(), dt.month()))),
            // Django's `o` — ISO-8601 week-numbering year (may
            // differ from Y near Jan 1 / Dec 31 boundaries).
            'o' => out.push_str(&format!("{:04}", dt.iso_week().year())),
            // Other Django codes we don't implement specially —
            // pass through as a literal (Django shape: unknown
            // chars are kept verbatim).
            other => out.push(other),
        }
    }
    out
}

/// Django-parity `dateformat.format(date, format_string)` for a
/// `NaiveDate` (no time component). Time-related format chars
/// (`H`, `i`, `s`, `a`, etc.) render as zeros to match what
/// Django does when given a `date` instead of `datetime`.
///
/// ```ignore
/// use chrono::NaiveDate;
/// use rustango::dateformat::format_date;
///
/// let d = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();
/// assert_eq!(format_date(&d, "Y-m-d"), "2026-06-04");
/// assert_eq!(format_date(&d, "D, F j Y"), "Thu, June 4 2026");
/// ```
#[must_use]
pub fn format_date(date: &NaiveDate, format_string: &str) -> String {
    let dt = date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc();
    format_datetime(&dt, format_string)
}

/// Django-parity `dateformat.time_format(time, format_string)` for
/// a `NaiveTime`. Date-related format chars (`Y`, `m`, `d`, `D`,
/// `l`, `N`, etc.) render against the Unix epoch date (1970-01-01)
/// when a time-only input is passed.
///
/// ```ignore
/// use chrono::NaiveTime;
/// use rustango::dateformat::time_format;
///
/// let t = NaiveTime::from_hms_opt(13, 5, 9).unwrap();
/// assert_eq!(time_format(&t, "H:i:s"), "13:05:09");
/// assert_eq!(time_format(&t, "g:i A"), "1:05 PM");
/// ```
#[must_use]
pub fn time_format(time: &NaiveTime, format_string: &str) -> String {
    let dt = NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .and_time(*time)
        .and_utc();
    format_datetime(&dt, format_string)
}

#[cfg(feature = "template_views")]
mod tera_filters {
    use super::{format_date, format_datetime, time_format};
    use crate::dateparse::{parse_date, parse_datetime, parse_time};
    use std::collections::HashMap;
    use tera::{to_value, Tera, Value};

    /// Register Django-shape date / time formatting filters on a Tera
    /// instance. The filters parse string inputs as ISO 8601
    /// datetime / date / time first, then apply the format string with
    /// Django single-char format codes (`Y` / `m` / `d` / `H` / `i` /
    /// `s` / `D` / `j` / `N` / `P` / etc., see [`format_datetime`]).
    ///
    /// Registered names:
    /// * `dateformat` — datetime → Django-formatted string
    /// * `timeformat` — time-only → Django-formatted string
    ///
    /// Filter usage in templates:
    /// ```jinja
    /// {{ post.created_at | dateformat("Y-m-d H:i") }}
    /// {{ event.starts_at | dateformat("D, F j Y") }}
    /// {{ schedule.time | timeformat("g:i A") }}
    /// ```
    ///
    /// Naming sidesteps Tera's built-in `date` filter (which uses
    /// chrono's strftime format) — that one stays available for users
    /// already using its `%Y`/`%m`/`%d` syntax.
    pub fn register_filters(tera: &mut Tera) {
        tera.register_filter("dateformat", dateformat);
        tera.register_filter("timeformat", timeformat);
    }

    fn fmt_arg(args: &HashMap<String, Value>) -> String {
        args.get("arg")
            .or_else(|| args.get("format"))
            .and_then(|v| v.as_str())
            .unwrap_or("Y-m-d H:i:s")
            .to_owned()
    }

    fn dateformat(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let fmt = fmt_arg(args);
        let s = match value {
            Value::String(s) => s.as_str(),
            _ => return Ok(value.clone()),
        };
        if let Some(dt) = parse_datetime(s) {
            return Ok(to_value(format_datetime(&dt, &fmt))?);
        }
        if let Some(d) = parse_date(s) {
            return Ok(to_value(format_date(&d, &fmt))?);
        }
        Ok(value.clone())
    }

    fn timeformat(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let fmt = fmt_arg(args);
        let s = match value {
            Value::String(s) => s.as_str(),
            _ => return Ok(value.clone()),
        };
        if let Some(t) = parse_time(s) {
            return Ok(to_value(time_format(&t, &fmt))?);
        }
        if let Some(dt) = parse_datetime(s) {
            return Ok(to_value(time_format(&dt.time(), &fmt))?);
        }
        Ok(value.clone())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn args(fmt: &str) -> HashMap<String, Value> {
            let mut m = HashMap::new();
            m.insert("arg".to_owned(), json!(fmt));
            m
        }

        #[test]
        fn dateformat_parses_iso_datetime_string() {
            let out = dateformat(&json!("2026-06-05T13:05:09Z"), &args("Y-m-d H:i")).unwrap();
            assert_eq!(out, json!("2026-06-05 13:05"));
        }

        #[test]
        fn dateformat_parses_date_only_string() {
            let out = dateformat(&json!("2026-06-05"), &args("D, F j Y")).unwrap();
            // June 5, 2026 was a Friday.
            assert_eq!(out, json!("Fri, June 5 2026"));
        }

        #[test]
        fn dateformat_default_format_when_arg_missing() {
            let out = dateformat(&json!("2026-06-05T13:05:09Z"), &HashMap::new()).unwrap();
            assert_eq!(out, json!("2026-06-05 13:05:09"));
        }

        #[test]
        fn dateformat_passes_through_non_string() {
            let out = dateformat(&json!(42), &args("Y-m-d")).unwrap();
            assert_eq!(out, json!(42));
        }

        #[test]
        fn dateformat_passes_through_unparseable() {
            let out = dateformat(&json!("not-a-date"), &args("Y-m-d")).unwrap();
            assert_eq!(out, json!("not-a-date"));
        }

        #[test]
        fn timeformat_parses_iso_time_string() {
            let out = timeformat(&json!("13:05:09"), &args("g:i A")).unwrap();
            assert_eq!(out, json!("1:05 PM"));
        }

        #[test]
        fn timeformat_falls_back_to_full_datetime() {
            let out = timeformat(&json!("2026-06-05T13:05:09Z"), &args("H:i:s")).unwrap();
            assert_eq!(out, json!("13:05:09"));
        }

        #[test]
        fn register_filters_wires_dateformat_through_tera() {
            let mut tera = Tera::default();
            register_filters(&mut tera);
            tera.add_raw_template("t", r#"{{ ts | dateformat(arg="Y-m-d") }}"#)
                .unwrap();
            let mut ctx = tera::Context::new();
            ctx.insert("ts", "2026-06-05T13:05:09Z");
            assert_eq!(tera.render("t", &ctx).unwrap(), "2026-06-05");
        }
    }
}

#[cfg(feature = "template_views")]
pub use tera_filters::register_filters;

fn short_month(m: u32) -> &'static str {
    match m {
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
        _ => "?",
    }
}

fn full_month(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "?",
    }
}

fn short_weekday(d: chrono::Weekday) -> &'static str {
    match d {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

fn full_weekday(d: chrono::Weekday) -> &'static str {
    match d {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    }
}

/// English ordinal suffix for a day-of-month (1st, 2nd, 3rd, 4th).
fn day_suffix(d: u32) -> &'static str {
    let n = d % 100;
    if (11..=13).contains(&n) {
        return "th";
    }
    match d % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Django's `P` — "p.m." / "a.m." with "noon" / "midnight" specials.
/// Examples: `(0, 0)` → `"midnight"`; `(12, 0)` → `"noon"`;
/// `(13, 0)` → `"1 p.m."`; `(13, 30)` → `"1:30 p.m."`.
fn fmt_pretty_meridiem(hour: u32, minute: u32) -> String {
    if hour == 0 && minute == 0 {
        return "midnight".to_owned();
    }
    if hour == 12 && minute == 0 {
        return "noon".to_owned();
    }
    let h12 = ((hour + 11) % 12) + 1;
    let meridiem = if hour < 12 { "a.m." } else { "p.m." };
    if minute == 0 {
        format!("{h12} {meridiem}")
    } else {
        format!("{h12}:{minute:02} {meridiem}")
    }
}

/// Django's `f` — 12-hour hour, plus `:MM` only when MM is
/// non-zero. Examples: `(13, 0)` → `"1"`; `(13, 30)` → `"1:30"`;
/// `(0, 0)` → `"12"`.
fn fmt_12h_minutes(hour: u32, minute: u32) -> String {
    let h12 = ((hour + 11) % 12) + 1;
    if minute == 0 {
        format!("{h12}")
    } else {
        format!("{h12}:{minute:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, s).unwrap()
    }

    #[test]
    fn format_y_m_d_h_i_s() {
        let t = dt(2026, 6, 4, 13, 5, 9);
        assert_eq!(format_datetime(&t, "Y-m-d H:i:s"), "2026-06-04 13:05:09");
    }

    #[test]
    fn format_year_codes() {
        let t = dt(2026, 1, 1, 0, 0, 0);
        assert_eq!(format_datetime(&t, "Y"), "2026");
        assert_eq!(format_datetime(&t, "y"), "26");
    }

    #[test]
    fn format_month_codes() {
        let t = dt(2026, 6, 4, 0, 0, 0);
        assert_eq!(format_datetime(&t, "m"), "06");
        assert_eq!(format_datetime(&t, "n"), "6");
        assert_eq!(format_datetime(&t, "M"), "Jun");
        assert_eq!(format_datetime(&t, "F"), "June");
        assert_eq!(format_datetime(&t, "b"), "jun");
    }

    #[test]
    fn format_day_codes() {
        let t = dt(2026, 6, 4, 0, 0, 0);
        assert_eq!(format_datetime(&t, "d"), "04");
        assert_eq!(format_datetime(&t, "j"), "4");
    }

    #[test]
    fn format_weekday_codes() {
        // 2026-06-04 was a Thursday.
        let t = dt(2026, 6, 4, 0, 0, 0);
        assert_eq!(format_datetime(&t, "D"), "Thu");
        assert_eq!(format_datetime(&t, "l"), "Thursday");
        assert_eq!(format_datetime(&t, "w"), "4"); // 0=Sun, 4=Thu
    }

    #[test]
    fn format_hour_codes_24h() {
        let t = dt(2026, 1, 1, 13, 0, 0);
        assert_eq!(format_datetime(&t, "H"), "13");
        assert_eq!(format_datetime(&t, "G"), "13");
    }

    #[test]
    fn format_hour_codes_12h() {
        // 13:00 → 1pm in 12h shape.
        let t = dt(2026, 1, 1, 13, 0, 0);
        assert_eq!(format_datetime(&t, "h"), "01");
        assert_eq!(format_datetime(&t, "g"), "1");
        // Midnight = 12am, not 00am.
        let m = dt(2026, 1, 1, 0, 0, 0);
        assert_eq!(format_datetime(&m, "h"), "12");
        assert_eq!(format_datetime(&m, "g"), "12");
        // Noon = 12pm.
        let n = dt(2026, 1, 1, 12, 0, 0);
        assert_eq!(format_datetime(&n, "h"), "12");
        assert_eq!(format_datetime(&n, "g"), "12");
    }

    #[test]
    fn format_am_pm() {
        let am = dt(2026, 1, 1, 9, 0, 0);
        let pm = dt(2026, 1, 1, 21, 0, 0);
        assert_eq!(format_datetime(&am, "a"), "am");
        assert_eq!(format_datetime(&am, "A"), "AM");
        assert_eq!(format_datetime(&pm, "a"), "pm");
        assert_eq!(format_datetime(&pm, "A"), "PM");
    }

    #[test]
    fn format_minute_second() {
        let t = dt(2026, 1, 1, 0, 5, 9);
        assert_eq!(format_datetime(&t, "i"), "05");
        assert_eq!(format_datetime(&t, "s"), "09");
    }

    #[test]
    fn format_day_of_year_z() {
        let t = dt(2026, 6, 4, 0, 0, 0);
        // June 4 of a non-leap year is day 155 — Django format code is `z`.
        assert_eq!(format_datetime(&t, "z"), "155");
    }

    #[test]
    fn format_day_of_year_z_leap_year() {
        // 2024 is a leap year; March 1 is day 61, December 31 is day 366.
        let mar1 = dt(2024, 3, 1, 0, 0, 0);
        assert_eq!(format_datetime(&mar1, "z"), "61");
        let dec31 = dt(2024, 12, 31, 0, 0, 0);
        assert_eq!(format_datetime(&dec31, "z"), "366");
    }

    #[test]
    // Named for the Django format code under test. Those codes are
    // case-sensitive — `N` (AP-style month) is a different specifier
    // from `n` (month number) — so lowercasing the suffix would make
    // the pair indistinguishable.
    #[allow(non_snake_case)]
    fn format_ap_month_abbr_N() {
        // Django format code `N` — AP-style month abbreviation: short
        // months get trailing period, long-form months (March, April,
        // May, June, July) render in full without period.
        assert_eq!(format_datetime(&dt(2026, 1, 1, 0, 0, 0), "N"), "Jan.");
        assert_eq!(format_datetime(&dt(2026, 2, 1, 0, 0, 0), "N"), "Feb.");
        assert_eq!(format_datetime(&dt(2026, 3, 1, 0, 0, 0), "N"), "March");
        assert_eq!(format_datetime(&dt(2026, 5, 1, 0, 0, 0), "N"), "May");
        assert_eq!(format_datetime(&dt(2026, 9, 1, 0, 0, 0), "N"), "Sept.");
        assert_eq!(format_datetime(&dt(2026, 12, 1, 0, 0, 0), "N"), "Dec.");
    }

    #[test]
    // Named for the Django format code under test. Those codes are
    // case-sensitive — `N` (AP-style month) is a different specifier
    // from `n` (month number) — so lowercasing the suffix would make
    // the pair indistinguishable.
    #[allow(non_snake_case)]
    fn format_unix_timestamp_U() {
        let t = dt(2026, 1, 1, 0, 0, 0);
        let expected = t.timestamp().to_string();
        assert_eq!(format_datetime(&t, "U"), expected);
    }

    #[test]
    fn format_iso_8601_c() {
        let t = dt(2026, 6, 4, 13, 5, 9);
        let out = format_datetime(&t, "c");
        assert!(out.starts_with("2026-06-04T13:05:09"));
        assert!(out.ends_with("+00:00"));
    }

    #[test]
    fn format_rfc_2822_r() {
        let t = dt(2026, 6, 4, 13, 5, 9);
        let out = format_datetime(&t, "r");
        // chrono RFC 2822 format includes weekday + GMT zone shape.
        assert!(out.contains("Thu, 4 Jun 2026 13:05:09"));
    }

    #[test]
    // Named for the Django format code under test. Those codes are
    // case-sensitive — `N` (AP-style month) is a different specifier
    // from `n` (month number) — so lowercasing the suffix would make
    // the pair indistinguishable.
    #[allow(non_snake_case)]
    fn format_day_suffix_S() {
        // 1 → st, 2 → nd, 3 → rd, 4 → th, 11 → th, 21 → st.
        assert_eq!(format_datetime(&dt(2026, 1, 1, 0, 0, 0), "S"), "st");
        assert_eq!(format_datetime(&dt(2026, 1, 2, 0, 0, 0), "S"), "nd");
        assert_eq!(format_datetime(&dt(2026, 1, 3, 0, 0, 0), "S"), "rd");
        assert_eq!(format_datetime(&dt(2026, 1, 4, 0, 0, 0), "S"), "th");
        assert_eq!(format_datetime(&dt(2026, 1, 11, 0, 0, 0), "S"), "th");
        assert_eq!(format_datetime(&dt(2026, 1, 21, 0, 0, 0), "S"), "st");
    }

    #[test]
    // Named for the Django format code under test. Those codes are
    // case-sensitive — `N` (AP-style month) is a different specifier
    // from `n` (month number) — so lowercasing the suffix would make
    // the pair indistinguishable.
    #[allow(non_snake_case)]
    fn format_leap_year_L() {
        assert_eq!(format_datetime(&dt(2024, 1, 1, 0, 0, 0), "L"), "1"); // leap
        assert_eq!(format_datetime(&dt(2026, 1, 1, 0, 0, 0), "L"), "0"); // not leap
        assert_eq!(format_datetime(&dt(2000, 1, 1, 0, 0, 0), "L"), "1"); // 2000 leap
        assert_eq!(format_datetime(&dt(1900, 1, 1, 0, 0, 0), "L"), "0"); // century not /400
    }

    #[test]
    fn format_timezone_codes_for_utc() {
        let t = dt(2026, 1, 1, 0, 0, 0);
        assert_eq!(format_datetime(&t, "O"), "+0000");
        assert_eq!(format_datetime(&t, "T"), "UTC");
        assert_eq!(format_datetime(&t, "e"), "UTC");
        assert_eq!(format_datetime(&t, "I"), "0");
        assert_eq!(format_datetime(&t, "Z"), "0");
    }

    #[test]
    fn format_escape_backslash() {
        let t = dt(2026, 6, 4, 0, 0, 0);
        // `\Y\e\a\r: Y` → literal "Year: 2026".
        assert_eq!(format_datetime(&t, r"\Y\e\a\r: Y"), "Year: 2026");
    }

    #[test]
    fn format_unknown_chars_pass_through() {
        let t = dt(2026, 6, 4, 0, 0, 0);
        // Hyphens, slashes, colons are not codes — passed verbatim.
        assert_eq!(format_datetime(&t, "Y/m/d"), "2026/06/04");
        assert_eq!(format_datetime(&t, "[Y]"), "[2026]");
    }

    #[test]
    fn format_empty_string() {
        let t = dt(2026, 1, 1, 0, 0, 0);
        assert_eq!(format_datetime(&t, ""), "");
    }

    #[test]
    fn format_microseconds_u() {
        let t = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap()
            .with_nanosecond(123_456_000)
            .unwrap();
        assert_eq!(format_datetime(&t, "u"), "123456");
    }

    // -------- format_date (NaiveDate) --------

    #[test]
    fn date_basic_iso() {
        let d = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();
        assert_eq!(format_date(&d, "Y-m-d"), "2026-06-04");
    }

    #[test]
    fn date_human_readable() {
        let d = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();
        assert_eq!(format_date(&d, "D, F j Y"), "Thu, June 4 2026");
    }

    #[test]
    fn date_time_codes_render_as_zeros() {
        // No time component → Django shape: render as midnight.
        let d = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();
        assert_eq!(format_date(&d, "H:i:s"), "00:00:00");
        assert_eq!(format_date(&d, "a"), "am");
    }

    // -------- time_format (NaiveTime) --------

    #[test]
    fn time_basic_24h() {
        let t = NaiveTime::from_hms_opt(13, 5, 9).unwrap();
        assert_eq!(time_format(&t, "H:i:s"), "13:05:09");
    }

    #[test]
    fn time_12h_with_am_pm() {
        let t = NaiveTime::from_hms_opt(13, 5, 0).unwrap();
        assert_eq!(time_format(&t, "g:i A"), "1:05 PM");
        let m = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
        assert_eq!(time_format(&m, "g:i A"), "9:30 AM");
    }

    #[test]
    fn time_date_codes_render_against_epoch() {
        // Time-only input → Django shape: date chars use 1970-01-01.
        let t = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        assert_eq!(time_format(&t, "Y-m-d"), "1970-01-01");
    }

    // -------- Django P / f / W / t / o (new codes) --------

    #[test]
    fn p_midnight_and_noon() {
        assert_eq!(format_datetime(&dt(2026, 6, 4, 0, 0, 0), "P"), "midnight");
        assert_eq!(format_datetime(&dt(2026, 6, 4, 12, 0, 0), "P"), "noon");
    }

    #[test]
    fn p_pretty_meridiem_with_and_without_minutes() {
        assert_eq!(format_datetime(&dt(2026, 6, 4, 13, 0, 0), "P"), "1 p.m.");
        assert_eq!(
            format_datetime(&dt(2026, 6, 4, 13, 30, 0), "P"),
            "1:30 p.m."
        );
        assert_eq!(format_datetime(&dt(2026, 6, 4, 9, 0, 0), "P"), "9 a.m.");
        assert_eq!(format_datetime(&dt(2026, 6, 4, 9, 5, 0), "P"), "9:05 a.m.");
    }

    #[test]
    fn f_collapses_zero_minutes() {
        assert_eq!(format_datetime(&dt(2026, 6, 4, 13, 0, 0), "f"), "1");
        assert_eq!(format_datetime(&dt(2026, 6, 4, 13, 30, 0), "f"), "1:30");
        assert_eq!(format_datetime(&dt(2026, 6, 4, 0, 0, 0), "f"), "12");
    }

    #[test]
    fn capital_w_iso_week_number() {
        // 2026-06-04 is in ISO week 23 of 2026.
        assert_eq!(format_datetime(&dt(2026, 6, 4, 0, 0, 0), "W"), "23");
        // 2026-01-01 is a Thursday — ISO week 1 of 2026.
        assert_eq!(format_datetime(&dt(2026, 1, 1, 0, 0, 0), "W"), "1");
    }

    #[test]
    fn t_days_in_month() {
        assert_eq!(format_datetime(&dt(2026, 1, 1, 0, 0, 0), "t"), "31");
        assert_eq!(format_datetime(&dt(2026, 2, 1, 0, 0, 0), "t"), "28");
        assert_eq!(format_datetime(&dt(2024, 2, 1, 0, 0, 0), "t"), "29"); // leap
        assert_eq!(format_datetime(&dt(2026, 4, 1, 0, 0, 0), "t"), "30");
        assert_eq!(format_datetime(&dt(2000, 2, 1, 0, 0, 0), "t"), "29"); // century leap
        assert_eq!(format_datetime(&dt(1900, 2, 1, 0, 0, 0), "t"), "28"); // century non-leap
    }

    #[test]
    fn o_iso_week_numbering_year() {
        // 2027-01-01 is a Friday → still in ISO week 53 of 2026.
        assert_eq!(format_datetime(&dt(2027, 1, 1, 0, 0, 0), "Y"), "2027");
        assert_eq!(format_datetime(&dt(2027, 1, 1, 0, 0, 0), "o"), "2026");
        // Mid-year is unambiguous.
        assert_eq!(format_datetime(&dt(2026, 6, 4, 0, 0, 0), "o"), "2026");
    }

    #[test]
    fn p_in_full_format_string() {
        let t = dt(2026, 6, 4, 13, 30, 0);
        assert_eq!(format_datetime(&t, "F j, Y, P"), "June 4, 2026, 1:30 p.m.");
    }
}
